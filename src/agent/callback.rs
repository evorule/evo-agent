// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G18:结构化事件回调
//!
//! 提供通用 callback 注册机制,让外部系统能对 agent 事件做反应,
//! 而不必消费整个 SSE 流。
//!
//! # 设计
//!
//! - [`EventCallback`] trait:异步 `on_event(&AgentEvent)`,覆盖所有事件变体
//! - [`CallbackChain`]:多回调广播,按序调用 + 超时保护(Q17 方案 C)
//! - [`LoggingCallback`]:把 AgentEvent 结构化日志输出(tracing)
//! - [`MetricsCallback`]:桥接 G17,把 AgentEvent 翻译成 metrics 计数
//!
//! # 回调执行模型(Q17 决议:方案 C — 同步等待 + 1s 超时)
//!
//! - 每个 callback 按注册顺序同步 await(保序)
//! - `tokio::time::timeout(1s, ...)` 防阻塞;超时记 warn,不中断 agent
//! - `catch_unwind` 防 panic;panic 记 error,不中断 agent
//!
//! # 集成方式
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use evo_agent::agent::{AgentRunner, AgentConfig, callback::LoggingCallback};
//! # use evo_agent::api::evorule_client::EvoruleApiClient;
//! # let runner = AgentRunner::new(AgentConfig::default(), EvoruleApiClient::new("http://localhost:8080"));
//! runner.with_event_callback(Arc::new(LoggingCallback::new()));
//! ```
//!
//! 回调在 `run_streaming()` 的每个事件 yield 点被调用。

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;

use crate::agent::runner::{AgentError, AgentEvent};
use crate::api::metrics::SharedMetrics;

/// 默认回调超时(Q17 方案 C:1s)
const DEFAULT_CALLBACK_TIMEOUT: Duration = Duration::from_secs(1);

/// 通用事件回调 —— 在 `run_streaming` 的每个 yield 点被调用
///
/// 与 G8 的 [`ApprovalCallback`](crate::agent::ApprovalCallback)(同步、只管审批)不同,
/// `EventCallback` 是异步的、覆盖所有 [`AgentEvent`] 变体。
/// 用于 metrics 插桩(G17)、webhook 通知、事件提取(G14)等场景。
///
/// # 实现约定
///
/// - `on_event` 不应阻塞主流程;重操作(如 HTTP webhook)应内部 spawn task 或用 channel 缓冲
/// - panic 会被 [`CallbackChain::dispatch`] 的 `catch_unwind` 捕获,不会中断 agent
#[async_trait::async_trait]
pub trait EventCallback: Send + Sync {
    /// 收到一个 agent 事件
    async fn on_event(&self, event: &AgentEvent);
}

/// 多回调广播 —— 注册多个 callback,按序调用
///
/// # 执行模型(Q17 方案 C)
///
/// - 每个 callback 同步 await(保序)
/// - 超时(`timeout` 秒,默认 1s):记 warn,跳过该 callback,继续下一个
/// - panic:记 error,跳过该 callback,继续下一个
///
/// # Clone 语义
///
/// `CallbackChain` 实现了 `Clone`(浅克隆 Vec<Arc<...>>),
/// 供 `Arc<CallbackChain>::make_mut` 在 `with_event_callback` 中使用。
#[derive(Clone)]
pub struct CallbackChain {
    callbacks: Vec<Arc<dyn EventCallback>>,
    timeout: Duration,
}

impl CallbackChain {
    /// 创建空回调链(默认 1s 超时)
    pub fn new() -> Self {
        Self {
            callbacks: Vec::new(),
            timeout: DEFAULT_CALLBACK_TIMEOUT,
        }
    }

    /// 设置回调超时(链式)
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 追加一个回调
    pub fn push(&mut self, cb: Arc<dyn EventCallback>) {
        self.callbacks.push(cb);
    }

    /// 是否没有注册任何回调
    pub fn is_empty(&self) -> bool {
        self.callbacks.is_empty()
    }

    /// 已注册的回调数量
    pub fn len(&self) -> usize {
        self.callbacks.len()
    }

    /// 按序调用所有 callback,每个加超时 + panic 保护
    ///
    /// - 超时:记 `tracing::warn`,跳过该 callback
    /// - panic:记 `tracing::error`,跳过该 callback
    /// - 不会因任何 callback 的故障而中断 agent 主流程
    pub async fn dispatch(&self, event: &AgentEvent) {
        for cb in &self.callbacks {
            let result = tokio::time::timeout(
                self.timeout,
                AssertUnwindSafe(cb.on_event(event)).catch_unwind(),
            )
            .await;
            match result {
                // 正常完成
                Ok(Ok(())) => {}
                // callback panicked
                Ok(Err(_)) => {
                    tracing::error!("G18: event callback panicked, skipping this callback");
                }
                // 超时
                Err(_) => {
                    tracing::warn!(
                        timeout_secs = self.timeout.as_secs(),
                        "G18: event callback timed out, skipping this callback"
                    );
                }
            }
        }
    }
}

impl Default for CallbackChain {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for CallbackChain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallbackChain")
            .field("callback_count", &self.callbacks.len())
            .field("timeout", &self.timeout)
            .finish()
    }
}

// =============================================================================
// 预置回调实现
// =============================================================================

/// 把 AgentEvent 结构化日志输出(tracing)
///
/// 每个事件变体映射到不同日志级别:
/// - `SessionCreated` / `LlmDone` / `ToolCall` / `ToolResult` / `Done` → `info`
/// - `Step` → `debug`
/// - `LlmDelta` → `trace`(高频,避免日志洪水)
/// - `ApprovalRequired` / `Error` → `warn` / `error`
pub struct LoggingCallback;

impl LoggingCallback {
    /// 创建 LoggingCallback
    pub fn new() -> Self {
        Self
    }
}

impl Default for LoggingCallback {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl EventCallback for LoggingCallback {
    async fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::SessionCreated { session_id } => {
                tracing::info!(%session_id, "callback: session created");
            }
            AgentEvent::Step { step } => {
                tracing::debug!(step, "callback: step");
            }
            AgentEvent::LlmDelta { text } => {
                tracing::trace!(len = text.len(), "callback: llm delta");
            }
            AgentEvent::LlmDone {
                content,
                finish_reason,
            } => {
                tracing::info!(
                    content_len = content.len(),
                    finish_reason = ?finish_reason,
                    "callback: llm done"
                );
            }
            AgentEvent::ToolCall { name, args } => {
                tracing::info!(%name, args = %args, "callback: tool call");
            }
            AgentEvent::ToolResult { name, result } => {
                tracing::info!(%name, result = %result, "callback: tool result");
            }
            AgentEvent::ApprovalRequired {
                tool_name, risk, ..
            } => {
                tracing::warn!(%tool_name, %risk, "callback: approval required");
            }
            AgentEvent::ApprovalResult {
                tool_name,
                approved,
            } => {
                tracing::info!(%tool_name, %approved, "callback: approval result");
            }
            AgentEvent::Info(msg) => {
                tracing::info!(%msg, "callback: info");
            }
            AgentEvent::Error(err) => {
                tracing::error!(error = %err, "callback: error");
            }
            AgentEvent::Done(result) => {
                tracing::info!(
                    success = result.success,
                    steps = result.steps,
                    duration_ms = result.duration_ms,
                    "callback: done"
                );
            }
        }
    }
}

/// 桥接 G17:把 AgentEvent 翻译成 metrics 计数
///
/// # ⚠️ 双重计数警告
///
/// 如果 runner 已通过 `with_metrics()` 注入了 G17 直插桩,
/// **不要再注册 `MetricsCallback`** —— 会导致 sessions/steps/llm_calls 等指标翻倍。
///
/// `MetricsCallback` 适用于:
/// - 不使用 `with_metrics()` 的场景(如 CLI 模式)
/// - 只想通过 callback 机制做事件计数(不含耗时直方图)
///
/// # 局限性
///
/// `AgentEvent` 不携带耗时和模型名信息,因此 `MetricsCallback` 只能计数,
/// 无法记录 `llm_duration_seconds` / `tool_duration_seconds` 直方图。
/// 需要完整指标(含耗时)时请用 `with_metrics()`。
pub struct MetricsCallback {
    metrics: SharedMetrics,
}

impl MetricsCallback {
    /// 创建 MetricsCallback
    pub fn new(metrics: SharedMetrics) -> Self {
        Self { metrics }
    }
}

#[async_trait::async_trait]
impl EventCallback for MetricsCallback {
    async fn on_event(&self, event: &AgentEvent) {
        match event {
            AgentEvent::SessionCreated { .. } => {
                self.metrics.inc_sessions_total();
            }
            AgentEvent::Step { .. } => {
                self.metrics.inc_steps();
            }
            AgentEvent::LlmDone { .. } => {
                // LLM 调用成功完成(收到 Done 事件)
                self.metrics.inc_llm_calls(true);
            }
            AgentEvent::Error(AgentError::LlmError(_)) => {
                // LLM 调用失败
                self.metrics.inc_llm_calls(false);
            }
            AgentEvent::ToolResult { name, result } => {
                // 从 result JSON 推断工具是否成功
                let ok = !is_tool_result_error(result);
                self.metrics.inc_tool_calls(name, ok);
            }
            // 其他事件不做 metrics 计数
            _ => {}
        }
    }
}

/// 从工具返回的 JSON 推断是否为错误
///
/// 约定:工具结果 JSON 如果包含 `status: "error"` 或 `error` 字段,视为失败。
fn is_tool_result_error(result: &serde_json::Value) -> bool {
    if let Some(obj) = result.as_object() {
        if obj.get("status").and_then(|v| v.as_str()) == Some("error") {
            return true;
        }
        if obj.get("error").is_some() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use crate::agent::runner::{AgentError, AgentEvent, AgentResult};
    use std::sync::Mutex;

    // ===== EventCallback mock =====

    /// 记录所有收到的事件(线程安全,供测试断言)
    struct RecordingCallback {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl RecordingCallback {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    events: events.clone(),
                },
                events,
            )
        }
    }

    #[async_trait::async_trait]
    impl EventCallback for RecordingCallback {
        async fn on_event(&self, event: &AgentEvent) {
            let label = match event {
                AgentEvent::SessionCreated { .. } => "session".to_string(),
                AgentEvent::Step { step } => format!("step:{}", step),
                AgentEvent::LlmDelta { .. } => "delta".to_string(),
                AgentEvent::LlmDone { .. } => "llm_done".to_string(),
                AgentEvent::ToolCall { name, .. } => format!("tool_call:{}", name),
                AgentEvent::ToolResult { name, .. } => format!("tool_result:{}", name),
                AgentEvent::Done(_) => "done".to_string(),
                AgentEvent::Error(_) => "error".to_string(),
                AgentEvent::Info(_) => "info".to_string(),
                AgentEvent::ApprovalRequired { .. } => "approval_required".to_string(),
                AgentEvent::ApprovalResult { .. } => "approval_result".to_string(),
            };
            self.events.lock().unwrap().push(label);
        }
    }

    // ===== CallbackChain 测试 =====

    #[test]
    fn test_callback_chain_new_is_empty() {
        let chain = CallbackChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
    }

    #[tokio::test]
    async fn test_dispatch_calls_all_callbacks_in_order() {
        let (cb1, events1) = RecordingCallback::new();
        let (cb2, events2) = RecordingCallback::new();
        let (cb3, events3) = RecordingCallback::new();

        let mut chain = CallbackChain::new();
        chain.push(Arc::new(cb1));
        chain.push(Arc::new(cb2));
        chain.push(Arc::new(cb3));

        let event = AgentEvent::SessionCreated {
            session_id: "s1".to_string(),
        };
        chain.dispatch(&event).await;

        assert_eq!(*events1.lock().unwrap(), vec!["session"]);
        assert_eq!(*events2.lock().unwrap(), vec!["session"]);
        assert_eq!(*events3.lock().unwrap(), vec!["session"]);
    }

    #[tokio::test]
    async fn test_dispatch_multiple_events() {
        let (cb, events) = RecordingCallback::new();
        let mut chain = CallbackChain::new();
        chain.push(Arc::new(cb));

        chain
            .dispatch(&AgentEvent::SessionCreated {
                session_id: "s1".to_string(),
            })
            .await;
        chain.dispatch(&AgentEvent::Step { step: 1 }).await;
        chain
            .dispatch(&AgentEvent::Done(AgentResult::success(
                "done".to_string(),
                1,
                100,
                vec![],
            )))
            .await;

        assert_eq!(*events.lock().unwrap(), vec!["session", "step:1", "done"]);
    }

    #[tokio::test]
    async fn test_dispatch_empty_chain_is_noop() {
        let chain = CallbackChain::new();
        let event = AgentEvent::SessionCreated {
            session_id: "s1".to_string(),
        };
        // Should not panic
        chain.dispatch(&event).await;
    }

    #[tokio::test]
    async fn test_timeout_does_not_block_dispatch() {
        /// 慢回调:sleep 10s(远超 1s 超时)
        struct SlowCallback;
        #[async_trait::async_trait]
        impl EventCallback for SlowCallback {
            async fn on_event(&self, _event: &AgentEvent) {
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }

        let (fast_cb, fast_events) = RecordingCallback::new();
        let mut chain = CallbackChain::new().with_timeout(Duration::from_millis(50));
        chain.push(Arc::new(SlowCallback));
        chain.push(Arc::new(fast_cb));

        let event = AgentEvent::Step { step: 1 };
        let start = std::time::Instant::now();
        chain.dispatch(&event).await;
        let elapsed = start.elapsed();

        // 慢回调被超时跳过,快回调仍然被调用
        assert!(
            elapsed < Duration::from_secs(2),
            "dispatch should not block on slow callback"
        );
        assert_eq!(*fast_events.lock().unwrap(), vec!["step:1"]);
    }

    #[tokio::test]
    async fn test_panic_does_not_propagate() {
        struct PanickingCallback;
        #[async_trait::async_trait]
        impl EventCallback for PanickingCallback {
            async fn on_event(&self, _event: &AgentEvent) {
                panic!("callback panic!");
            }
        }

        let (after_cb, after_events) = RecordingCallback::new();
        let mut chain = CallbackChain::new().with_timeout(Duration::from_secs(5));
        chain.push(Arc::new(PanickingCallback));
        chain.push(Arc::new(after_cb));

        let event = AgentEvent::SessionCreated {
            session_id: "s1".to_string(),
        };
        // Should not panic
        chain.dispatch(&event).await;

        // 后续 callback 仍然被调用
        assert_eq!(*after_events.lock().unwrap(), vec!["session"]);
    }

    // ===== LoggingCallback 测试 =====

    #[tokio::test]
    async fn test_logging_callback_does_not_panic() {
        let cb = LoggingCallback::new();
        // 遍历所有 AgentEvent 变体,确保不 panic
        let events = vec![
            AgentEvent::SessionCreated {
                session_id: "s1".to_string(),
            },
            AgentEvent::Step { step: 1 },
            AgentEvent::LlmDelta {
                text: "hello".to_string(),
            },
            AgentEvent::LlmDone {
                content: "done".to_string(),
                finish_reason: Some("stop".to_string()),
            },
            AgentEvent::ToolCall {
                name: "file_read".to_string(),
                args: serde_json::json!({}),
            },
            AgentEvent::ToolResult {
                name: "file_read".to_string(),
                result: serde_json::json!({"status": "ok"}),
            },
            AgentEvent::ApprovalRequired {
                tool_name: "shell_exec".to_string(),
                command: "rm -rf".to_string(),
                risk: "high".to_string(),
                alternative: "use trash".to_string(),
            },
            AgentEvent::ApprovalResult {
                tool_name: "shell_exec".to_string(),
                approved: false,
            },
            AgentEvent::Info("rewind".to_string()),
            AgentEvent::Error(AgentError::LlmError("timeout".to_string())),
            AgentEvent::Done(AgentResult::success("ok".to_string(), 1, 100, vec![])),
        ];
        for event in &events {
            cb.on_event(event).await;
        }
    }

    // ===== MetricsCallback 测试 =====

    fn make_metrics() -> SharedMetrics {
        Arc::new(crate::api::metrics::Metrics::new().unwrap())
    }

    #[tokio::test]
    async fn test_metrics_callback_session_and_step() {
        let metrics = make_metrics();
        let cb = MetricsCallback::new(metrics.clone());

        cb.on_event(&AgentEvent::SessionCreated {
            session_id: "s1".to_string(),
        })
        .await;
        cb.on_event(&AgentEvent::Step { step: 1 }).await;
        cb.on_event(&AgentEvent::Step { step: 2 }).await;

        let output = metrics.render();
        assert!(output.contains("evo_agent_sessions_total 1"));
        assert!(output.contains("evo_agent_steps_total 2"));
    }

    #[tokio::test]
    async fn test_metrics_callback_llm_calls() {
        let metrics = make_metrics();
        let cb = MetricsCallback::new(metrics.clone());

        // 成功的 LLM 调用
        cb.on_event(&AgentEvent::LlmDone {
            content: "result".to_string(),
            finish_reason: Some("stop".to_string()),
        })
        .await;
        // 失败的 LLM 调用
        cb.on_event(&AgentEvent::Error(AgentError::LlmError(
            "timeout".to_string(),
        )))
        .await;

        let output = metrics.render();
        assert!(output.contains("evo_agent_llm_calls_total{status=\"ok\"} 1"));
        assert!(output.contains("evo_agent_llm_calls_total{status=\"error\"} 1"));
    }

    #[tokio::test]
    async fn test_metrics_callback_tool_calls() {
        let metrics = make_metrics();
        let cb = MetricsCallback::new(metrics.clone());

        // 成功的工具调用
        cb.on_event(&AgentEvent::ToolResult {
            name: "file_read".to_string(),
            result: serde_json::json!({"status": "ok", "content": "hello"}),
        })
        .await;
        // 失败的工具调用(有 error 字段)
        cb.on_event(&AgentEvent::ToolResult {
            name: "shell_exec".to_string(),
            result: serde_json::json!({"error": "permission denied"}),
        })
        .await;

        let output = metrics.render();
        // Prometheus 0.13 按 label 名字母序:status < tool
        assert!(output.contains("evo_agent_tool_calls_total{status=\"ok\",tool=\"file_read\"} 1"));
        assert!(
            output.contains("evo_agent_tool_calls_total{status=\"error\",tool=\"shell_exec\"} 1")
        );
    }

    #[tokio::test]
    async fn test_metrics_callback_ignores_irrelevant_events() {
        let metrics = make_metrics();
        let cb = MetricsCallback::new(metrics.clone());

        // 这些事件不应触发任何 metrics 计数
        cb.on_event(&AgentEvent::LlmDelta {
            text: "delta".to_string(),
        })
        .await;
        cb.on_event(&AgentEvent::ToolCall {
            name: "file_read".to_string(),
            args: serde_json::json!({}),
        })
        .await;
        cb.on_event(&AgentEvent::Info("info".to_string())).await;
        cb.on_event(&AgentEvent::ApprovalRequired {
            tool_name: "shell_exec".to_string(),
            command: "rm".to_string(),
            risk: "high".to_string(),
            alternative: "trash".to_string(),
        })
        .await;

        let output = metrics.render();
        // 没有任何 counter 被递增(非 Vec 指标默认为 0)
        assert!(output.contains("evo_agent_sessions_total 0"));
        assert!(output.contains("evo_agent_steps_total 0"));
    }

    // ===== is_tool_result_error 测试 =====

    #[test]
    fn test_is_tool_result_error_with_status_field() {
        assert!(is_tool_result_error(
            &serde_json::json!({"status": "error"})
        ));
        assert!(!is_tool_result_error(&serde_json::json!({"status": "ok"})));
    }

    #[test]
    fn test_is_tool_result_error_with_error_field() {
        assert!(is_tool_result_error(
            &serde_json::json!({"error": "failed"})
        ));
    }

    #[test]
    fn test_is_tool_result_error_no_error() {
        assert!(!is_tool_result_error(
            &serde_json::json!({"content": "hello"})
        ));
        assert!(!is_tool_result_error(&serde_json::Value::Null));
        assert!(!is_tool_result_error(&serde_json::json!("string")));
    }

    // ===== Clone / Debug 测试 =====

    #[test]
    fn test_callback_chain_clone() {
        let (cb, _) = RecordingCallback::new();
        let mut chain = CallbackChain::new();
        chain.push(Arc::new(cb));
        let cloned = chain.clone();
        assert_eq!(chain.len(), cloned.len());
    }

    #[test]
    fn test_callback_chain_debug() {
        let chain = CallbackChain::new();
        let debug = format!("{:?}", chain);
        assert!(debug.contains("CallbackChain"));
        assert!(debug.contains("callback_count"));
    }
}
