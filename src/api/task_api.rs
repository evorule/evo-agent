// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 任务面端点:
//!
//! - `GET  /api/tasks`                    列工作区 tasks.json 任务(serve 端解析校验)
//! - `POST /api/tasks/run`                {label} → 校验 → spawn → run_id
//! - `POST /api/tasks/{run_id}/cancel`    两段式取消(优雅→窗口→强杀树)
//! - `GET  /api/tasks/ws?run_id=&token=`  事件流(回放+活播;G16 token fallback)
//!
//! 治理分级:任务由人的显式动作发起(永不自动运行);首跑确认在前端;
//! task.started/ended/cancelled(含命令原文)经 WS gov_event 留痕(record-only)。

#![forbid(unsafe_code)]

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
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
use crate::tasks::runner::{self, TaskSpec};
use crate::tasks::MAX_CONCURRENT;

/// WS 单帧上限
const WS_MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// tasks.json 形态(工作区 .evo/tasks.json)
#[derive(Debug, Deserialize)]
pub struct TasksFile {
    /// 任务定义清单
    #[serde(default)]
    pub tasks: Vec<TaskSpec>,
}

fn tasks_json_path(state: &AgentApiState) -> std::path::PathBuf {
    state.workdir().join(".evo").join("tasks.json")
}

/// `GET /api/tasks` — 列任务
pub async fn list_tasks(State(state): State<AgentApiState>) -> Response {
    let path = tasks_json_path(&state);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => {
            return Json(json!({
                "tasks": [],
                "error": null,
                "hint": format!("未找到任务配置({}):请手工创建 tasks.json", path.display()),
            }))
            .into_response()
        }
    };
    match serde_json::from_str::<TasksFile>(&text) {
        Ok(file) => {
            let tasks: Vec<serde_json::Value> = file
                .tasks
                .iter()
                .map(|t| {
                    json!({
                        "label": t.label,
                        "type": if t.is_shell() { "shell" } else { "process" },
                        "commandDisplay": t.command_display(),
                        "matchers": t.matchers(),
                    })
                })
                .collect();
            Json(json!({ "tasks": tasks, "error": null })).into_response()
        }
        Err(e) => Json(json!({
            "tasks": [],
            "error": format!("tasks.json 解析失败: {e}"),
        }))
        .into_response(),
    }
}

/// `POST /api/tasks/run` — 运行任务
pub async fn run_task(
    State(state): State<AgentApiState>,
    Json(body): Json<serde_json::Value>,
) -> Response {
    let Some(label) = body.get("label").and_then(|v| v.as_str()) else {
        return (StatusCode::BAD_REQUEST, "missing label".to_string()).into_response();
    };
    let tasks = match read_task_specs(&state) {
        Ok(t) => t,
        Err(e) => return (StatusCode::NOT_FOUND, e).into_response(),
    };
    let Some(spec) = tasks.iter().find(|t| t.label == label) else {
        return (StatusCode::NOT_FOUND, format!("任务不存在: {label}")).into_response();
    };
    // 并发护栏:总数 + 同 label 互斥
    if state.task_manager().active_summaries().len() >= MAX_CONCURRENT {
        return (
            StatusCode::CONFLICT,
            format!("并发任务已达上限({MAX_CONCURRENT})"),
        )
            .into_response();
    }
    if state.task_manager().is_label_running(label) {
        return (StatusCode::CONFLICT, format!("任务 {label} 正在运行中")).into_response();
    }

    // spawn + 事件编排
    let (line_tx, mut line_rx) = mpsc::channel::<(String, String)>(256);
    let (cancel_tx, mut cancel_rx) = mpsc::channel::<u32>(4);
    let (btx, _) = tokio::sync::broadcast::channel::<String>(2048);
    let workdir = state.workdir().to_path_buf();
    let spec = spec.clone();

    let child = match runner::spawn_task(&spec, &workdir, line_tx).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let pid = child.id();
    let command_display = spec.command_display();
    let label_owned = label.to_string();
    let matchers: Vec<String> = spec.matchers().to_vec();

    // 注册运行(回放缓冲+活播广播+取消通道)+启动/治理事件
    let manager = state.task_manager().clone();
    let run_id = manager.register(&label_owned, &command_display, pid, cancel_tx.clone(), btx);
    let owner = format!("task.{label_owned}");
    if let Some(run) = manager.get(&run_id) {
        let gov_started = json!({
            "type": "gov_event",
            "event": { "action": "task.started", "label": label_owned, "command": command_display },
        });
        run.emit(gov_started.to_string());
        let started_event = json!({
            "type": "started", "runId": run_id,
            "label": label_owned, "command": command_display,
        });
        run.emit(started_event.to_string());
    }
    info!(run_id, label = %label_owned, pid, "task spawned: {command_display}");

    // 事件编排任务:持有 child,轮询步进(输出 try_recv 全排/取消 try_recv/
    // try_wait 退出检测;100ms 步进,免 select! 宏嵌套解析问题)
    let mgr = manager.clone();
    let rid = run_id.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let mut child = child;
        let mut matcher_buf: Vec<crate::tasks::matcher::MatchedDiag> = Vec::new();
        let mut cancel_phase: u32 = 0;
        let mut grace_deadline: Option<std::time::Instant> = None;
        let mut exit_code: i32 = -1;

        loop {
            // 1) 排干全部就绪输出行
            loop {
                match line_rx.try_recv() {
                    Ok((stream, text)) => {
                        let Some(run) = mgr.get(&rid) else { continue };
                        let out_event = json!({
                            "type": "output", "runId": rid,
                            "stream": stream, "line": text,
                        });
                        run.emit(out_event.to_string());
                        // matcher 增量解析(首个命中即批推累计集,前端全量替换同 owner)
                        for m in &matchers {
                            if let Some(d) = crate::tasks::matcher::parse_line(m, &text) {
                                matcher_buf.push(d);
                                let diag_event = json!({
                                    "type": "diagnostics", "runId": rid,
                                    "owner": owner,
                                    "items": matcher_buf.clone(),
                                });
                                run.emit(diag_event.to_string());
                                break;
                            }
                        }
                    }
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(mpsc::error::TryRecvError::Disconnected) => break,
                }
            }
            // 2) 取消信号(非阻塞;首次=优雅,再次=强杀)
            loop {
                match cancel_rx.try_recv() {
                    Ok(p) if p > 1 || cancel_phase > 0 => {
                        runner::cancel_two_phase(pid, 2);
                        cancel_phase = 2;
                        grace_deadline = None;
                    }
                    Ok(_) => {
                        runner::cancel_two_phase(pid, 1);
                        cancel_phase = 1;
                        grace_deadline =
                            Some(std::time::Instant::now() + runner::GRACE_KILL_WINDOW);
                    }
                    Err(_) => break,
                }
            }
            // 3) 优雅窗到期→第二段强杀
            if let Some(deadline) = grace_deadline {
                if std::time::Instant::now() >= deadline {
                    runner::cancel_two_phase(pid, 2);
                    cancel_phase = 2;
                    grace_deadline = None;
                }
            }
            // 4) 退出检测(非阻塞)
            match child.try_wait() {
                Ok(Some(status)) => {
                    exit_code = status.code().unwrap_or(-1);
                    break;
                }
                Ok(None) => {}
                Err(e) => {
                    warn!(run_id = %rid, error = %e, "task wait failed");
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }

        // 收尾:排干残余输出行+ended 事件+gov 留痕
        while let Ok((stream, text)) = line_rx.try_recv() {
            if let Some(run) = mgr.get(&rid) {
                let out_event = json!({
                    "type": "output", "runId": rid,
                    "stream": stream, "line": text,
                });
                run.emit(out_event.to_string());
            }
        }
        let cancelled = cancel_phase > 0;
        if let Some(run) = mgr.get(&rid) {
            run.finish(exit_code, cancelled);
            let ended_event = json!({
                "type": "ended", "runId": rid,
                "exitCode": exit_code,
                "durationMs": started.elapsed().as_millis() as u64,
                "cancelled": cancelled,
            });
            run.emit(ended_event.to_string());
            let action = if cancelled {
                "task.cancelled"
            } else {
                "task.ended"
            };
            let gov_event = json!({
                "type": "gov_event",
                "event": {
                    "action": action,
                    "label": run.label.clone(),
                    "command": run.command_display.clone(),
                    "exitCode": exit_code,
                },
            });
            run.emit(gov_event.to_string());
        }
        info!(run_id = %rid, exit_code, cancelled, "task ended");
    });

    Json(json!({ "runId": run_id, "pid": pid })).into_response()
}

/// `POST /api/tasks/{run_id}/cancel` — 取消(两段式第一段)
pub async fn cancel_task(
    State(state): State<AgentApiState>,
    Path(run_id): Path<String>,
) -> Response {
    match state.task_manager().cancel(&run_id) {
        Some(true) => Json(json!({ "cancelled": true })).into_response(),
        Some(false) => (StatusCode::CONFLICT, "run already finished".to_string()).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown run_id".to_string()).into_response(),
    }
}

/// 任务 WS query 参数
#[derive(Debug, Deserialize)]
pub struct TaskWsQuery {
    /// 运行 id
    pub run_id: Option<String>,
    /// 鉴权 token(auth 启用时必填)
    pub token: Option<String>,
}

/// `GET /api/tasks/ws?run_id=...` — 事件流(回放+活播)
pub async fn task_ws(
    ws: WebSocketUpgrade,
    State(state): State<AgentApiState>,
    Query(query): Query<TaskWsQuery>,
) -> Response {
    if state.auth_config().enabled() {
        let ok = query
            .token
            .as_deref()
            .map(|t| state.auth_config().validate(t))
            .unwrap_or(false);
        if !ok {
            warn!("task WS upgrade rejected — invalid token");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    }
    let Some(run_id) = query.run_id else {
        return (StatusCode::BAD_REQUEST, "missing run_id".to_string()).into_response();
    };
    ws.max_message_size(WS_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            handle_stream(socket, state, run_id).await;
        })
}

async fn handle_stream(socket: WebSocket, state: AgentApiState, run_id: String) {
    let (mut sink, _stream) = socket.split(); // 上行无输入面(v1 单向事件流)
    let manager = state.task_manager();
    let Some(run) = manager.get(&run_id) else {
        let _ = sink
            .send(WsMessage::Text(
                json!({"type":"error","hint":"unknown run_id"})
                    .to_string()
                    .into(),
            ))
            .await;
        return;
    };
    // 回放缓冲(断连续流语义)
    for event in run.replay_snapshot() {
        if sink.send(WsMessage::Text(event.into())).await.is_err() {
            return;
        }
    }
    if run.is_finished() {
        return; // 已结束:回放即全量
    }
    // 活播
    let mut rx = run.subscribe();
    loop {
        match rx.recv().await {
            Ok(event) => {
                let is_ended = event.contains("\"type\":\"ended\"");
                if sink.send(WsMessage::Text(event.into())).await.is_err() {
                    return;
                }
                if is_ended {
                    return; // ended 事件即收尾(单 run 单流)
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                debug!("task ws lagged, {n} events skipped");
            }
            Err(_) => return, // 发送端消亡(run 回收)
        }
    }
}

/// 读取并解析 tasks.json(共 list/run 两用)
fn read_task_specs(state: &AgentApiState) -> Result<Vec<TaskSpec>, String> {
    let path = tasks_json_path(state);
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("未找到任务配置({}):请手工创建", path.display()))?;
    let file: TasksFile =
        serde_json::from_str(&text).map_err(|e| format!("tasks.json 解析失败: {e}"))?;
    Ok(file.tasks)
}
