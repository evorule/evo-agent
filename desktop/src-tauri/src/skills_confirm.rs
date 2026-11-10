// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 技能管理动作原生确认通道（两段式确认的壳侧承载面）：
//!
//! 工作台收到 202 挑战后经 Tauri IPC 调入本模块——**裁决对话框是壳原生
//! 窗口，物理隔离在 webview 沙箱之外**：偷到 webview 令牌的调用方可以
//! 签发挑战，但无法自动化「人按了哪个按钮」。
//!
//! 会话级授权（裁定口径）：每壳引导首枚挑战弹一次「允许本会话技能变更」，
//! 允许 → 壳置位 + 通知 serve（后续动作直接执行，不再有挑战）；拒绝 →
//! 回落逐次确认（每动作一问）。授权态随壳进程存续（引导级），serve 侧
//! 同步置位（serve 重启丢授权时由下一枚挑战重新裁决补齐）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

/// 会话授权已裁决（首枚挑战的一次性问询闸）
static GRANT_DECIDED: AtomicBool = AtomicBool::new(false);
/// 会话授权在位（true=后续动作静默放行）
static SESSION_GRANT: AtomicBool = AtomicBool::new(false);

/// 原生双选对话框（阻塞式；在线程池上运行避免占用 async 执行器）
fn ask(app: &AppHandle, title: &str, msg: &str, yes: &str, no: &str) -> bool {
    app.dialog()
        .message(msg.to_string())
        .title(title.to_string())
        .buttons(tauri_plugin_dialog::MessageDialogButtons::OkCancelCustom(
            yes.to_string(),
            no.to_string(),
        ))
        .blocking_show()
}

/// `skills_native_confirm` —— 工作台对 202 挑战的裁决入口。
/// 返回 `{executed, state, result|detail}`：executed=true 时 result=原
/// 动作响应体（approve/revoke/import 的正常返回）。
#[tauri::command]
pub async fn skills_native_confirm(
    app: AppHandle,
    challenge_id: String,
    action: String,
) -> Result<serde_json::Value, String> {
    let sup = app
        .state::<Arc<crate::supervisor::Supervisor>>()
        .inner()
        .clone();
    let token = sup.serve_token.clone();
    let port = sup
        .nav_port
        .read()
        .ok()
        .and_then(|p| *p)
        .ok_or_else(|| "workbench port unknown (serve not ready)".to_string())?;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    // 会话级授权：每壳引导首枚挑战一次裁决（裁定口径）
    if !GRANT_DECIDED.load(Ordering::SeqCst) {
        let app2 = app.clone();
        let allow_session = tauri::async_runtime::spawn_blocking(move || {
            ask(
                &app2,
                "技能变更确认",
                "工作台请求进行技能变更（批准 / 撤销 / 导入技能包）。\n\n允许本会话的所有技能变更？\n（选择「允许本会话」后本会话内不再逐次询问；选择「每次询问」则每次单独确认）",
                "允许本会话",
                "每次询问",
            )
        })
        .await
        .map_err(|e| e.to_string())?;
        GRANT_DECIDED.store(true, Ordering::SeqCst);
        if allow_session {
            SESSION_GRANT.store(true, Ordering::SeqCst);
            // 通知 serve 置位（本枚挑战已签发，仍需 confirm 消费）
            let _ = client
                .post(format!("{base}/api/skills/confirm-session-grant"))
                .bearer_auth(&token)
                .json(&serde_json::json!({ "enable": true }))
                .send()
                .await;
        }
    }

    if SESSION_GRANT.load(Ordering::SeqCst) {
        // 授权在位：静默消费（serve 重启丢授权时顺带重新置位）
        let rearm = client
            .post(format!("{base}/api/skills/confirm-session-grant"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "enable": true }))
            .send()
            .await;
        if rearm.is_err() {
            return Err("serve unreachable for confirm".to_string());
        }
        return consume(client, base, token, challenge_id).await;
    }

    // 逐次确认（会话授权未授予）
    let app2 = app.clone();
    let allow = tauri::async_runtime::spawn_blocking(move || {
        ask(
            &app2,
            "技能变更确认",
            &format!("工作台请求执行技能管理动作：{action}。\n允许执行本次操作？"),
            "允许",
            "拒绝",
        )
    })
    .await
    .map_err(|e| e.to_string())?;
    if !allow {
        let resp = client
            .post(format!("{base}/api/skills/confirm-deny"))
            .bearer_auth(&token)
            .json(&serde_json::json!({ "challenge_id": challenge_id }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let detail: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
        return Ok(serde_json::json!({
            "executed": false,
            "state": "denied",
            "detail": detail,
        }));
    }
    consume(client, base, token, challenge_id).await
}

/// 消费挑战（confirm 端点；壳持特权令牌）
async fn consume(
    client: reqwest::Client,
    base: String,
    token: String,
    challenge_id: String,
) -> Result<serde_json::Value, String> {
    let resp = client
        .post(format!("{base}/api/skills/confirm"))
        .bearer_auth(&token)
        .json(&serde_json::json!({ "challenge_id": challenge_id }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Ok(serde_json::json!({
            "executed": false,
            "state": "error",
            "detail": body,
        }));
    }
    Ok(serde_json::json!({
        "executed": true,
        "state": "executed",
        "result": body,
    }))
}
