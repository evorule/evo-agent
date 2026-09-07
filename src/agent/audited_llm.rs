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
//!   sidecar 会话与三者解耦，且一次一会话无共享状态、天然并发安全。
//!
//! ## 失败语义
//!
//! 无静默直连兜底。server 不可达/协议失败时如实返回 `Err`，由调用方既有的
//! best-effort 语义承接（如 G10 保留原 hint）。三个生产调用点全部位于依赖
//! server 的运行周期内，直连兜底不带来新可用性，只会制造不可审计数据。

use std::time::Duration;

use tracing::{info, warn};

use crate::api::api_core::ApiError;
use crate::api::evorule_client::EvoruleApiClient;
use crate::io_handler::IoHandler;
use crate::io_handlers::LlmHandler;
use evorule_tcb::JsonValue;

/// 单次审计调用的整体超时（含建会话 + LLM 执行 + 协议回路）
///
/// 主流程 step_timeout 默认量级参考；摘要类调用输入较大，取 90s。
/// 该超时作用于每个等待点（HTTP 请求 / 下一个事件），并非全周期硬上限。
pub const DEFAULT_AUDITED_CALL_TIMEOUT_SECS: u64 = 90;

/// F2（audit-chain 专项 2026-08-28）：建链阶段（create_session / subscribe_events）
/// 对瞬态错误的有界重试次数。语义为"审计链缺段比多一次请求更贵"——连接类
/// 抖动不应直接造成审计链缺失。命令提交与事件回路阶段**不重试**（避免 LLM
/// 重复执行副作用，保持既有 fail-fast 语义）。
pub const SIDECAR_SETUP_RETRIES: u32 = 1;

/// 建链重试间隔
const SIDECAR_SETUP_RETRY_DELAY: Duration = Duration::from_millis(500);

/// 建链阶段瞬态错误判定
///
/// - HTTP 语义错误：仅 5xx 视为瞬态（服务端暂态故障）；4xx（认证失败、
///   路径错误等）重试不会成功，不重试；
/// - 连接类错误（`HttpError`：连接拒绝 / DNS / 超时）与响应格式异常
///   （`InvalidResponse` / `SerializationError`）视为瞬态。
fn is_transient_setup_error(err: &ApiError) -> bool {
    match err {
        ApiError::ApiError { status, .. } => *status >= 500,
        ApiError::SessionNotFound | ApiError::InvalidVersion(_) => false,
        ApiError::HttpError(_) | ApiError::InvalidResponse | ApiError::SerializationError(_) => {
            true
        }
    }
}

/// 建链步骤的统一包装：整体超时 + 瞬态错误有界重试（F2）
///
/// 重试发生时 `warn!` 留痕——重试本身也是审计信息（调用方 best-effort
/// 语义可在日志侧看到"曾经历 N 次建链尝试"）。
async fn setup_with_retry<T, F, Fut>(
    deadline: Duration,
    mut op: F,
    step: &str,
    purpose: &str,
) -> Result<T, String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ApiError>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match tokio::time::timeout(deadline, op()).await {
            Err(_) => {
                if attempt <= SIDECAR_SETUP_RETRIES {
                    warn!(
                        purpose,
                        step, attempt, "audited_llm: setup timed out, retrying"
                    );
                    tokio::time::sleep(SIDECAR_SETUP_RETRY_DELAY).await;
                    continue;
                }
                return Err(format!(
                    "audited_llm[{purpose}]: timed out {step} (after {attempt} attempts)"
                ));
            }
            Ok(Err(e)) if is_transient_setup_error(&e) && attempt <= SIDECAR_SETUP_RETRIES => {
                warn!(purpose, step, attempt, error = %e, "audited_llm: setup transient error, retrying");
                tokio::time::sleep(SIDECAR_SETUP_RETRY_DELAY).await;
            }
            Ok(Err(e)) => return Err(format!("audited_llm[{purpose}]: {step}: {e}")),
            Ok(Ok(v)) => return Ok(v),
        }
    }
}

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

        // 1. 一次性 sidecar 会话（F2：瞬态错误有界重试，语义错误直接失败）
        let session_id = setup_with_retry(
            deadline,
            || self.client.create_session(None),
            "create_session",
            purpose,
        )
        .await?;

        // 2. 必须先订阅再提交命令（broadcast 通道不重放历史，与主循环同因）
        let mut events = setup_with_retry(
            deadline,
            || self.client.subscribe_events(&session_id),
            "subscribe_events",
            purpose,
        )
        .await?;

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
                    let (response_value, error_msg): (serde_json::Value, Option<String>) =
                        match &exec_result {
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
    serde_json::from_str(&v.to_string()).map_err(|e| format!("convert tcb json to serde_json: {e}"))
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

    /// F2 回归：create_session 返回 5xx（瞬态）→ 重试 1 次后成功，
    /// 全协议走通；且重试请求确实发出（第一个 mock 命中 1 次）。
    #[tokio::test]
    async fn test_execute_retries_create_session_on_transient_error() {
        let mut server = mockito::Server::new_async().await;
        let audited = AuditedLlm::new(
            EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"ok"}"#),
        );

        // 第一次：500 瞬态错误；第二次：成功（mockito 按注册顺序匹配）
        let create_fail = server
            .mock("POST", "/api/sessions")
            .with_status(503)
            .with_body("server busy")
            .create_async()
            .await;
        let create_ok = server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":91}"#)
            .create_async()
            .await;
        let sse = concat!(
            "data: {\"type\":\"IoRequest\",\"id\":7}\n\n",
            "data: {\"type\":\"Stable\"}\n\n"
        );
        let events_mock = server
            .mock("GET", "/api/sessions/91/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        let command_mock = server
            .mock("POST", "/api/sessions/91/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        let io_response_mock = server
            .mock("POST", "/api/sessions/91/io_response")
            .match_body(mockito::Matcher::PartialJson(json!({"request_id": 7})))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let params = tcb(&json!({
            "model": "mm",
            "temperature": 0.0,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let result = audited.execute("summarize", &params).await.unwrap();
        assert!(result.to_string().contains("ok"));

        create_fail.assert_async().await;
        create_ok.assert_async().await;
        events_mock.assert_async().await;
        command_mock.assert_async().await;
        io_response_mock.assert_async().await;
    }

    /// F2 对照：create_session 返回 4xx（语义错误）→ 不重试，一次即失败。
    #[tokio::test]
    async fn test_execute_no_retry_on_semantic_error() {
        let mut server = mockito::Server::new_async().await;
        let audited = AuditedLlm::new(
            EvoruleApiClient::new(&server.url()),
            LlmHandler::mock(r#"{"content":"unused"}"#),
        );

        let create_reject = server
            .mock("POST", "/api/sessions")
            .with_status(401)
            .with_body("unauthorized")
            .expect(1)
            .create_async()
            .await;

        let params = tcb(&json!({
            "model": "mm",
            "temperature": 0.0,
            "messages": [{"role": "user", "content": "hi"}],
        }));
        let err = audited.execute("summarize", &params).await.unwrap_err();
        assert!(
            err.contains("create_session"),
            "错误信息应指明建链阶段: {err}"
        );
        assert!(!err.contains("attempts"), "语义错误不应显示重试次数: {err}");

        create_reject.assert_async().await;
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

        let err = audited
            .execute("rollup", &tcb(&json!({"messages": []})))
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

        let err = audited
            .execute("session_summary", &tcb(&json!({"messages": []})))
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
        let err = audited
            .execute("summarize", &tcb(&json!({"messages": []})))
            .await
            .unwrap_err();
        assert!(err.contains("create_session"), "got: {err}");
    }

    // ===== 协议参数契约锁定（公共设施专项 2026-08-30：防无声变更） =====

    /// 超时与建链重试次数是 sidecar 协议的显式契约（见模块 doc 与 memory 台账），
    /// 变更必须是有意识的协议升级，此处锁定防止无声漂移。
    #[test]
    fn test_protocol_constants_locked() {
        assert_eq!(DEFAULT_AUDITED_CALL_TIMEOUT_SECS, 90);
        assert_eq!(SIDECAR_SETUP_RETRIES, 1);
    }

    /// is_transient_setup_error 全分支：
    /// 5xx / 连接类 / 响应格式 / 序列化 → 瞬态（重试）；
    /// 4xx / 会话类（SessionNotFound、InvalidVersion）→ 语义错误（不重试）。
    #[tokio::test]
    async fn test_transient_setup_error_all_branches() {
        // 瞬态：5xx
        assert!(is_transient_setup_error(&ApiError::ApiError {
            status: 503,
            message: "busy".into(),
        }));
        // 语义：4xx
        assert!(!is_transient_setup_error(&ApiError::ApiError {
            status: 401,
            message: "unauthorized".into(),
        }));
        // 瞬态：连接类 / 序列化（#[from] 包装，用真实错误实例构造）
        let http_err = reqwest::get("http://127.0.0.1:1").await.unwrap_err();
        assert!(is_transient_setup_error(&ApiError::HttpError(http_err)));
        let ser_err = serde_json::from_str::<serde_json::Value>("{bad").unwrap_err();
        assert!(is_transient_setup_error(&ApiError::SerializationError(
            ser_err
        )));
        // 瞬态：响应格式
        assert!(is_transient_setup_error(&ApiError::InvalidResponse));
        // 语义：会话类
        assert!(!is_transient_setup_error(&ApiError::SessionNotFound));
        assert!(!is_transient_setup_error(&ApiError::InvalidVersion(
            "v-1".into()
        )));
    }

    /// LLM 本地执行失败 → 错误写进 io_response（引擎状态机收尾，不留悬空 IoRequest），
    /// 随后如实返回 Err。这是"LLM 失败不留悬挂 IoRequest"契约的回归测试。
    #[tokio::test]
    async fn test_execute_llm_failure_writes_error_io_response() {
        let mut server = mockito::Server::new_async().await;
        // LLM 指向不可达地址 → llm.execute 必失败
        let failing_llm = LlmHandler::new("mm", "http://127.0.0.1:1", None).with_max_retries(0);

        let audited = AuditedLlm::new(EvoruleApiClient::new(&server.url()), failing_llm);

        server
            .mock("POST", "/api/sessions")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(r#"{"session_id":12}"#)
            .create_async()
            .await;
        let sse = concat!(
            "data: {\"type\":\"IoRequest\",\"id\":3}\n\n",
            "data: {\"type\":\"Stable\"}\n\n"
        );
        server
            .mock("GET", "/api/sessions/12/events")
            .with_status(200)
            .with_header("content-type", "text/event-stream")
            .with_body(sse)
            .create_async()
            .await;
        server
            .mock("POST", "/api/sessions/12/command")
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;
        // 关键断言：io_response 必须被调用（错误也要回写，不能悬空）
        let io_response_mock = server
            .mock("POST", "/api/sessions/12/io_response")
            .match_body(mockito::Matcher::PartialJson(json!({"request_id": 3})))
            .with_status(200)
            .with_body("{}")
            .create_async()
            .await;

        let err = audited
            .execute(
                "summarize",
                &tcb(&json!({"messages": [{"role":"user","content":"hi"}]})),
            )
            .await
            .unwrap_err();
        assert!(err.contains("llm execute"), "错误应来自 LLM 执行: {err}");
        io_response_mock.assert_async().await;
    }
}
