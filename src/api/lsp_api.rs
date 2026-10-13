// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LSP 代理端点:语言门状态检测 + WS 透明转发升级。
//!
//! - `GET /api/lsp/status`:两语言门 PATH 检测(可用性+版本),前端灰置降级依据
//! - `GET /api/lsp/ws?lang=rust|typescript&token=`:WS 升级,接通
//!   [`crate::lsp::bridge`] 桥(每语言单连接单实例,占用中拒绝新连接)
//!
//! 鉴权与既有 WS 端点同款双层:auth 中间件第一层(含 `?token=` fallback),
//! handler 内二次校验(defense in depth)。

#![forbid(unsafe_code)]

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
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::api::agent_api::AgentApiState;
use crate::lsp::bridge::BridgeEvent;
use crate::lsp::Lang;

/// WS 帧大小上限(设计裁定 4MB;超限 axum 断开,前端以断连语义处理)。
/// LS→client 方向的大帧(全量诊断)实测在 KB-MB 级;Full 同步的单文件
/// didOpen/ didChange 大于此上限的文件(>4MB 源码)会断链重连,属已知边界。
const WS_MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// 单语言门检测状态
#[derive(Debug, serde::Serialize)]
pub struct GateStatus {
    /// PATH 上可找到且 --version 可执行
    pub available: bool,
    /// 版本行首行(如 `rust-analyzer 0.3.x`)
    pub version: Option<String>,
    /// 缺失/不可用时的安装引导(v1 不托管下载,只引导)
    pub hint: Option<String>,
}

/// `GET /api/lsp/status` 响应
#[derive(Debug, serde::Serialize)]
pub struct LspStatusResponse {
    /// Rust 门(rust-analyzer)
    pub rust: GateStatus,
    /// TypeScript 门(typescript-language-server,兼管 javascript)
    pub typescript: GateStatus,
    /// 工作区根绝对路径(POSIX 风格;前端构造真实 file:// 文档 URI 与 rootUri 用;
    /// 与 /version 同口径,仅鉴权面内暴露)
    pub workdir: String,
}

/// WS 连接 query 参数
#[derive(Debug, Deserialize)]
pub struct LspWsQuery {
    /// 语言门(`rust` | `typescript`)
    pub lang: Option<String>,
    /// 鉴权 token(auth 启用时必填)
    pub token: Option<String>,
}

/// 探测一个命令的可用性与版本(`<prog> --version`,5s 超时;候选名同 spawn)。
async fn probe_version(program: &str, args: &[&str]) -> GateStatus {
    use crate::lsp::process::program_candidates;
    let mut last_err_text = String::new();
    for candidate in program_candidates(program) {
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new(&candidate).args(args).output(),
        )
        .await;
        match output {
            Ok(Ok(out)) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                let version = text
                    .lines()
                    .next()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty());
                return GateStatus {
                    available: true,
                    version,
                    hint: None,
                };
            }
            Ok(Ok(out)) => {
                // 命令存在但退出码非零:候选名命中,不再尝试其余形态
                return GateStatus {
                    available: false,
                    version: None,
                    hint: Some(format!(
                        "{program} 存在但退出码非零({});检查安装完整性",
                        out.status
                    )),
                };
            }
            Ok(Err(e)) => {
                last_err_text = e.to_string(); // NotFound:试下一候选
            }
            Err(_) => {
                return GateStatus {
                    available: false,
                    version: None,
                    hint: Some(format!("{program} --version 探测超时(5s);检查安装")),
                };
            }
        }
    }
    let _ = last_err_text;
    GateStatus {
        available: false,
        version: None,
        hint: Some(install_hint(program)),
    }
}

fn install_hint(program: &str) -> String {
    if program == "rust-analyzer" {
        "rust-analyzer 未安装:执行 `rustup component add rust-analyzer`".into()
    } else {
        "typescript-language-server 未安装:执行 `npm install -g typescript typescript-language-server`".into()
    }
}

/// `GET /api/lsp/status` — 两语言门检测(每次调用实时探测;轻量子进程,低频面)
pub async fn status(State(state): State<AgentApiState>) -> Json<LspStatusResponse> {
    let (rust, typescript) = tokio::join!(
        probe_version("rust-analyzer", &["--version"]),
        probe_version("typescript-language-server", &["--version"]),
    );
    // 绝对化:相对 workdir(如 ".")对前端 file:// URI 构造不可用;Windows
    // canonicalize 产生 `\\?\` verbatim 前缀,按 fs_watch 同款口径剥除
    let workdir_abs = state
        .workdir()
        .canonicalize()
        .unwrap_or_else(|_| state.workdir().to_path_buf());
    let workdir_raw = workdir_abs.to_string_lossy().to_string();
    let workdir = workdir_raw
        .strip_prefix(r"\\?\")
        .unwrap_or(&workdir_raw)
        .replace('\\', "/");
    Json(LspStatusResponse {
        rust,
        typescript,
        workdir,
    })
}

/// `GET /api/lsp/ws?lang=...&token=...` — WS 升级
pub async fn lsp_ws(
    ws: WebSocketUpgrade,
    State(state): State<AgentApiState>,
    Query(query): Query<LspWsQuery>,
) -> Response {
    // 语言门参数
    let lang = match query.lang.as_deref().and_then(Lang::from_query) {
        Some(l) => l,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                "missing or invalid ?lang= (rust | typescript)",
            )
                .into_response()
        }
    };
    // 二次鉴权(defense in depth — 中间件已做第一层)
    if state.auth_config().enabled() {
        let ok = query
            .token
            .as_deref()
            .map(|t| state.auth_config().validate(t))
            .unwrap_or(false);
        if !ok {
            warn!("LSP WS upgrade rejected — invalid token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    // 每语言单连接单实例
    if !state.lsp_registry().try_acquire(lang) {
        return (
            StatusCode::CONFLICT,
            format!(
                "{} gate already has an active LSP connection",
                lang.as_str()
            ),
        )
            .into_response();
    }

    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| handle_socket(socket, state, lang))
}

async fn handle_socket(socket: WebSocket, state: AgentApiState, lang: Lang) {
    let (mut sink, mut stream) = socket.split();
    let (in_tx, in_rx) = mpsc::channel::<String>(128);
    let (out_tx, mut out_rx) = mpsc::channel::<BridgeEvent>(128);

    // 入向泵:WS 文本帧 → 桥(二进制帧 LSP over WS 不使用;忽略)
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

    // 桥本体(spawn 语言服务器 + 泵 + 重启治理);结束时 drop out_tx
    let workdir = state.workdir().to_path_buf();
    let bridge = tokio::spawn(async move {
        crate::lsp::bridge::run_bridge(lang, workdir, in_rx, out_tx).await;
    });

    // 出向泵:桥事件 → WS 文本帧
    while let Some(ev) = out_rx.recv().await {
        let text = match ev {
            BridgeEvent::Text(t) => t,
            BridgeEvent::State { state, detail } => {
                info!(gate = lang.as_str(), lsp_state = state, "{detail}");
                json!({"type": "lsp_state", "lang": lang.as_str(), "state": state, "detail": detail})
                    .to_string()
            }
            BridgeEvent::Log { level, message } => {
                debug!(gate = lang.as_str(), level = %level, "[lsp] {message}");
                json!({"type": "lsp_log", "lang": lang.as_str(), "level": level, "message": message})
                    .to_string()
            }
            BridgeEvent::Unavailable { hint } => {
                warn!(gate = lang.as_str(), "language server unavailable: {hint}");
                json!({"type": "lsp_unavailable", "lang": lang.as_str(), "hint": hint}).to_string()
            }
        };
        if sink.send(WsMessage::Text(text.into())).await.is_err() {
            break; // 客户端已断开;桥将经入向通道关闭自行收尾
        }
    }

    // 收尾:桥退出(out_tx drop → 本循环结束)或出向断(中止桥并等收尸)
    bridge.abort();
    let _ = bridge.await;
    in_task.abort();
    let _ = in_task.await;
    state.lsp_registry().release(lang);
    debug!(gate = lang.as_str(), "LSP WS connection cleaned up");
}
