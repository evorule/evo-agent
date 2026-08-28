// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! P2-V3 结构性修复（2026-08-27）：审计链内 LLM 调用桥
//!
//! ## 问题
//!
//! `ContextSummarizer` 等辅助 LLM 调用此前直连 `LlmHandler`，prompt/response
//! 完全不经 evorule fact 流程（"影子调用"，research/issues P2-V3）。
//! 止血阶段以旁路计数器量化盲区；本模块将主路径迁入审计链，影子调用归零。
//!
//! ## 协议：一次性 sidecar 会话
//!
//! 每次调用创建独立的 evorule 会话，走与 ReAct 主循环完全相同的
//! `call_external` 契约：
//!
//! ```text
//! create_session → subscribe_events → submit_command(call_external)
//!     └─ 命令事实入审计链（承载 prompt 全文）
//! IoRequest 事件 → 取 request_id → 本地执行 llm.execute → submit_io_response
//!     └─ io_response 事实入审计链（承载结果全文）
//! Stable 事件 → 返回本地执行结果
//! ```
//!
//! 不复用主会话的原因：
//! - `summarize_dropped` 发生在服务主会话 IoRequest **期间**，引擎状态机
//!   正在等待 io_response，嵌套提交命令会死锁；
//! - `summarize_session`/`rollup_summaries` 发生在 Stable 之后，终态语义下
//!   是否接受新命令未经契约验证。
//! sidecar 会话与三者解耦，且一次一会话无共享状态、天然并发安全。
//!
//! ## 失败语义
//!
//! 无静默直连兜底。server 不可达/协议失败时如实返回 `Err`，由调用方既有的
//! best-effort 语义承接（如 G10 保留原 hint）。三个生产调用点全部位于依赖
//! server 的运行周期内，直连兜底不带来新可用性，只会制造不可审计数据。

use std::time::Duration;

use tracing::{info, warn};

use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;
use evorule_tcb::JsonValue;

/// 单次审计调用的整体超时（含建会话 + LLM 执行 + 协议回路）
///
/// 主流程 step_timeout 默认量级参考；摘要类调用输入较大，取 90s。
/// 该超时作用于每个等待点（HTTP 请求 / 下一个事件），并非全周期硬上限。
pub const DEFAULT_AUDITED_CALL_TIMEOUT_SECS: u64 = 90;

/// 审计链内 LLM 执行器 —— 把"影子调用"迁入 evorule fact 流程
#[derive(Debug, Clone)]
pub struct AuditedLlm {
    client: EvoruleApiClient,
    llm: LlmHandler,
    timeout_secs: u64,
}

impl AuditedLlm {
    /// 创建审计执行器
    ///
    /// - `client`：evorule 客户端（与主流程同实例即可）
    /// - `llm`：实际执行 LLM 调用的 handler（与直连路径共用）
    pub fn new(client: EvoruleApiClient, llm: LlmHandler) -> Self {
        Self {
            client,
            llm,
            timeout_secs: DEFAULT_AUDITED_CALL_TIMEOUT_SECS,
        }
    }

    /// 自定义整体超时（秒）
    pub fn with_timeout_secs(mut self, secs: u64) -> Self {
        self.timeout_secs = secs;
        self
    }

    /// 在审计链内执行一次 LLM 调用
    ///
    /// - `purpose`：用途标签（summarize / session_summary / rollup），作为
    ///   `audit_purpose` 写入命令事实，供审计侧区分调用类别
    /// - `params`：`call_external` 参数对象（model / temperature / max_tokens /
    ///   messages），与直连路径构造方式完全一致；经命令事实进入审计链
    ///
    /// 成功返回本地 LLM 执行结果（不经服务端 payload 回读，避免存储形态耦合）。
    pub async fn execute(&self, purpose: &str, params: &JsonValue) -> Result<JsonValue, String> {
        let deadline = Duration::from_secs(self.timeout_secs);

        // 1. 一次性 sidecar 会话
        let session_id = tokio::time::timeout(deadline, self.client.create_session(None))
            .await
            .map_err(|_| format!("audited_llm[{purpose}]: timed out creating session"))?
            .map_err(|e| format!("audited_llm[{purpose}]: create_session: {e}"))?;

        // 2. 必须先订阅再提交命令（broadcast 通道不重放历史，与主循环同因）
        let mut events = tokio::time::timeout(deadline, self.client.subscribe_events(&session_id))
            .await
            .map_err(|_| format!("audited_llm[{purpose}]: timed out subscribing events"))?
            .map_err(|e| format!("audited_llm[{purpose}]: subscribe_events: {e}"))?;

        // 3. 提交 call_external 命令 —— prompt 经命令事实进入审计链
        let command = build_call_external_command(purpose, params)?;
        self.client
            .submit_command(&session_id, &command)
            .await
            .map_err(|e| format!("audited_llm[{purpose}]: submit_command: {e}"))?;
        info!(%session_id, %purpose, "audited_llm: sidecar command submitted");

        // 4. 事件回路：IoRequest → 本地执行 → io_response；Stable → 完成
        let mut llm_result: Option<JsonValue> = None;
        loop {
            let event = match tokio::time::timeout(deadline, events.next()).await {
                Err(_) => {
                    return Err(format!(
                        "audited_llm[{purpose}]: timed out waiting for event (session {session_id})"
                    ));
                }
                Ok(None) => {
                    return Err(format!(
                        "audited_llm[{purpose}]: event stream closed before Stable (session {session_id})"
                    ));
                }
                Ok(Some(event)) => event,
            };

            match event.event_type.as_str() {
                "IoRequest" => {
                    let request_id = event
                        .payload
                        .get("id")
                        .and_then(|v| v.as_u64())
                        .ok_or_else(|| {
                            format!(
                                "audited_llm[{purpose}]: IoRequest missing id (session {session_id})"
                            )
                        })?;
                    info!(%session_id, request_id, purpose, "audited_llm: executing LLM locally");

                    // 本地执行；失败也要把错误写进 io_response 再返回，
                    // 保证引擎状态机能收尾（不留悬空 IoRequest）
                    let exec_result = self.llm.execute(params).await;
                    let (response_value, error_msg): (
                        serde_json::Value,
                        Option<String>,
                    ) = match &exec_result {
                        Ok(v) => match serde_json_value_of(v) {
                            Ok(value) => (value, None),
                            Err(e) => (serde_json::json!({ "error": e }), Some(e)),
                        },
                        Err(e) => (serde_json::json!({ "error": e }), Some(e.clone())),
                    };
                    self.client
                        .submit_io_response(
                            &session_id,
                            request_id,
                            &response_value,
                            error_msg.as_deref(),
                        )
                        .await
                        .map_err(|e| format!("audited_llm[{purpose}]: submit_io_response: {e}"))?;
                    match exec_result {
                        Ok(v) => llm_result = Some(v),
                        Err(e) => return Err(format!("audited_llm[{purpose}]: llm execute: {e}")),
                    }
                }
                "Stable" => {
                    info!(%session_id, purpose, "audited_llm: audited call complete");
                    return llm_result.ok_or_else(|| {
                        format!(
                            "audited_llm[{purpose}]: reached Stable without executing LLM \
                             (session {session_id})"
                        )
                    });
                }
                "Error" => {
                    let msg = event
                        .payload
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("engine reported Error");
                    warn!(%session_id, purpose, error = %msg, "audited_llm: engine error");
                    return Err(format!("audited_llm[{purpose}]: engine error: {msg}"));
                }
                _ => {} // StateTransition 等其他事件忽略
            }
        }
    }
}

/// 构造 `call_external` 命令
///
/// params 原样透传为 `instruction.params`（core_eval 规则引用
/// `instruction.params.messages`，缺失会导致 path resolution failed）；
/// `audit_purpose` 作为额外键写入 params —— 规则不读取该键，
/// 但它随命令事实持久化进审计链，供审计侧区分调用类别。
fn build_call_external_command(
    purpose: &str,
    params: &JsonValue,
) -> Result<serde_json::Value, String> {
    let mut p = serde_json_value_of(params)?;
    match &mut p {
        serde_json::Value::Object(map) => {
            map.insert("audit_purpose".to_string(), serde_json::json!(purpose));
        }
        _ => {
            return Err(format!(
                "audited_llm[{purpose}]: call params must be a JSON object"
            ));
        }
    }
    Ok(serde_json::json!({ "type": "call_external", "params": p }))
}

/// JsonValue(TCB) → serde_json::Value
///
/// 沿用既有风格（to_string + parse）：TCB JSON 的文本形式是合法 JSON，
/// 解析结果一一对应。解析失败如实报错，不静默降级为 Null。
fn serde_json_value_of(v: &JsonValue) -> Result<serde_json::Value, String> {
    serde_json::from_str(&v.to_string())
        .map_err(|e| format!("convert tcb json to serde_json: {e}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use serde_json::json;

    fn tcb(v: &serde_json::Value) -> JsonValue {
        crate::json_convert::serde_to_tcb(v)
    }

    // ===== 命令构造单元测试 =====

    #[test]
    fn test_command_passthrough_and_purpose() {
        let params = json!({
            "model": "m",
            "temperature": 0.0,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let cmd = build_call_external_command("summarize", &tcb(&params)).unwrap();
        assert_eq!(cmd["type"], "call_external");
        // core_eval 规则契约路径：messages 必须位于 instruction.params 下（原样透传）
        assert_eq!(cmd["params"]["messages"][0]["content"], "hi");
        assert_eq!(cmd["params"]["model"], "m");
        // purpose 写入 params 额外键，随命令事实入审计链
        assert_eq!(cmd["params"]["audit_purpose"], "summarize");
    }

    #[test]
    fn test_command_rejects_non_object_params() {
        let p = tcb(&json!("not-an-object"));
        let err = build_call_external_command("x", &p).unwrap_err();
        assert!(err.contains("must be a JSON object"));
    }

    // ===== sidecar 协议回路集成测试 =====

    /// 完整协议：create_session → events(IoRequest+Stable) → command → io_response
    #[tokio::test]
    async fn test_execute_happy_path_full_protocol() {
        let mut server = mockito::Server::new_async().await;
        let audited = AuditedLlm::new(
            EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"hello summary"}"#),
        );

        let create_mock = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":77}"#)
            .create_async()
            .await;
        let sse = concat!(
            "data: {\"type\":\"IoRequest\",\"id\":5}\n\n",
            "data: {\"type\":\"Stable\"}\n\n"
        );
        let events_mock = server
            .mock("GET", "/api/sessions/77/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        let command_mock = server
            .mock("POST", "/api/sessions/77/command")
            .match_body(mockito::Matcher::PartialJson(json!({
                "instruction": {
                    "type": "call_external",
                    "params": {"audit_purpose": "summarize"}
                }
            })))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let io_response_mock = server
            .mock("POST", "/api/sessions/77/io_response")
            .match_body(mockito::Matcher::PartialJson(json!({"request_id": 5})))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let params = tcb(&json!({
            "model": "mm",
            "temperature": 0.0,
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let result = audited.execute("summarize", &params).await.unwrap();
        assert!(result.to_string().contains("hello summary"));

        create_mock.assert_async().await;
        events_mock.assert_async().await;
        command_mock.assert_async().await;
        io_response_mock.assert_async().await;
    }

    /// 流在 Stable 前关闭 → 如实报错（不留悬挂等待）
    #[tokio::test]
    async fn test_execute_err_when_stream_closes_before_stable() {
        let mut server = mockito::Server::new_async().await;
        let audited = AuditedLlm::new(
            EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"unused"}"#),
        );

        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":8}"#)
            .create_async()
            .await;
        // 只有 IoRequest，随后流关闭
        let sse = "data: {\"type\":\"IoRequest\",\"id\":1}\n\n";
        server
            .mock("GET", "/api/sessions/8/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/8/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/8/io_response")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let err = audited.execute("rollup", &tcb(&json!({"messages": []})))
            .await
            .unwrap_err();
        assert!(err.contains("closed before Stable"), "got: {err}");
    }

    /// 引擎 Error 事件 → 如实上抛错误消息
    #[tokio::test]
    async fn test_execute_propagates_engine_error_event() {
        let mut server = mockito::Server::new_async().await;
        let audited = AuditedLlm::new(
            EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"unused"}"#),
        );

        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":9}"#)
            .create_async()
            .await;
        let sse = "data: {\"type\":\"Error\",\"message\":\"path resolution failed\"}\n\n";
        server
            .mock("GET", "/api/sessions/9/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/9/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let err = audited.execute("session_summary", &tcb(&json!({"messages": []})))
            .await
            .unwrap_err();
        assert!(err.contains("path resolution failed"), "got: {err}");
    }

    /// server 不可达 → 如实报错（无静默直连兜底）
    #[tokio::test]
    async fn test_execute_honest_failure_on_unreachable_server() {
        // 端口 1 保留端口,连接立即可靠地被拒绝
        let audited = AuditedLlm::new(
            EvoruleApiClient::new("http://127.0.0.1:1"),
            LlmHandler::mock(r#"{"content":"should never be reached"}"#),
        );
        let err = audited.execute("summarize", &tcb(&json!({"messages": []})))
            .await
            .unwrap_err();
        assert!(err.contains("create_session"), "got: {err}");
    }
}
