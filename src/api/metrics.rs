// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G17:Prometheus 指标模块
//!
//! 定义 evo-agent 的运行时指标,通过 `/metrics` 端点暴露给 Prometheus 抓取。
//!
//! # 设计
//!
//! 使用**自定义 Registry**(非全局),与 evorule 核心模式一致,支持测试隔离。
//! `SharedMetrics = Arc<Metrics>`,在 `AgentApiState` 和 `AgentRunner` 间共享。
//!
//! # 指标列表
//!
//! | 指标 | 类型 | Labels | 说明 |
//! |------|------|--------|------|
//! | `evo_agent_sessions_total` | Counter | — | session 创建总数 |
//! | `evo_agent_sessions_active` | Gauge | — | 当前活跃 session 数 |
//! | `evo_agent_llm_calls_total` | Counter | `status` | LLM 调用次数(ok/error) |
//! | `evo_agent_llm_duration_seconds` | Histogram | `model` | LLM 调用耗时分布 |
//! | `evo_agent_tool_calls_total` | Counter | `tool`,`status` | 工具调用次数 |
//! | `evo_agent_tool_duration_seconds` | Histogram | `tool` | 工具调用耗时分布 |
//! | `evo_agent_steps_total` | Counter | — | 执行步数 |
//! | `evo_agent_sse_connections` | Gauge | — | 当前 SSE 连接数 |

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use prometheus::{
    HistogramOpts, HistogramVec, IntCounter, IntCounterVec, IntGauge, Opts, Registry, TextEncoder,
};

/// 指标创建错误
#[derive(Debug)]
pub enum MetricsError {
    /// Gauge 指标创建失败
    GaugeCreationFailed(String),
    /// Counter 指标创建失败
    CounterCreationFailed(String),
    /// Histogram 指标创建失败
    HistogramCreationFailed(String),
    /// 指标注册到 Registry 失败
    RegistryRegistrationFailed(String),
}

impl fmt::Display for MetricsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MetricsError::GaugeCreationFailed(name) => {
                write!(f, "Failed to create gauge: {}", name)
            }
            MetricsError::CounterCreationFailed(name) => {
                write!(f, "Failed to create counter: {}", name)
            }
            MetricsError::HistogramCreationFailed(name) => {
                write!(f, "Failed to create histogram: {}", name)
            }
            MetricsError::RegistryRegistrationFailed(name) => {
                write!(f, "Failed to register metric: {}", name)
            }
        }
    }
}

impl std::error::Error for MetricsError {}

/// evo-agent 运行时指标集合
///
/// 持有独立的 `Registry`(非全局),便于测试隔离。
/// 所有指标在 `new()` 时注册到 registry,之后通过 `render()` 输出 Prometheus 文本格式。
pub struct Metrics {
    registry: Registry,
    sessions_total: IntCounter,
    sessions_active: IntGauge,
    llm_calls_total: IntCounterVec,
    llm_duration_seconds: HistogramVec,
    tool_calls_total: IntCounterVec,
    tool_duration_seconds: HistogramVec,
    steps_total: IntCounter,
    sse_connections: IntGauge,
    /// P2-V3 止血（2026-08-27）：Summarizer 等直连 LLM 绕过审计链的调用计数
    ///
    /// prompt/response 不经 evorule fact 流程的"影子调用"被指标化，
    /// 使审计盲区的大小可量化。按用途打标签（如 summarize/rollup）。
    llm_bypass_audit_total: IntCounterVec,
    /// P5-A3 指标（2026-08-27）：L2 SafetyAuditor 召回内容审计命中计数
    ///
    /// 按命中规则打标签，使召回污染态势可告警（此前仅 tracing::warn 单通道）。
    safety_audit_hits_total: IntCounterVec,
    /// B3 指标（2026-08-28）：memory cache 与真相源（evorule）漂移条目计数
    ///
    /// 定期校验对齐时累计漂移条目数（ghost 清理 + miss 回填），
    /// 持续增长说明写入链路存在系统性失败。
    memory_cache_drift_total: IntCounter,
}

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Metrics").finish()
    }
}

impl Metrics {
    /// 创建并注册所有指标
    pub fn new() -> Result<Self, MetricsError> {
        let registry = Registry::new();

        let sessions_total =
            IntCounter::new("evo_agent_sessions_total", "Total agent sessions created").map_err(
                |_| MetricsError::CounterCreationFailed("evo_agent_sessions_total".into()),
            )?;
        let sessions_active =
            IntGauge::new("evo_agent_sessions_active", "Current active agent sessions").map_err(
                |_| MetricsError::GaugeCreationFailed("evo_agent_sessions_active".into()),
            )?;
        let llm_calls_total = IntCounterVec::new(
            Opts::new("evo_agent_llm_calls_total", "Total LLM calls by status"),
            &["status"],
        )
        .map_err(|_| MetricsError::CounterCreationFailed("evo_agent_llm_calls_total".into()))?;
        let llm_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "evo_agent_llm_duration_seconds",
                "LLM call duration in seconds by model",
            )
            .buckets(vec![0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0]),
            &["model"],
        )
        .map_err(|_| {
            MetricsError::HistogramCreationFailed("evo_agent_llm_duration_seconds".into())
        })?;
        let tool_calls_total = IntCounterVec::new(
            Opts::new(
                "evo_agent_tool_calls_total",
                "Total tool calls by tool and status",
            ),
            &["tool", "status"],
        )
        .map_err(|_| MetricsError::CounterCreationFailed("evo_agent_tool_calls_total".into()))?;
        let tool_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "evo_agent_tool_duration_seconds",
                "Tool call duration in seconds by tool",
            )
            .buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
            ]),
            &["tool"],
        )
        .map_err(|_| {
            MetricsError::HistogramCreationFailed("evo_agent_tool_duration_seconds".into())
        })?;
        let steps_total =
            IntCounter::new("evo_agent_steps_total", "Total agent execution steps")
                .map_err(|_| MetricsError::CounterCreationFailed("evo_agent_steps_total".into()))?;
        let sse_connections = IntGauge::new(
            "evo_agent_sse_connections",
            "Current active SSE connections",
        )
        .map_err(|_| MetricsError::GaugeCreationFailed("evo_agent_sse_connections".into()))?;
        let llm_bypass_audit_total = IntCounterVec::new(
            Opts::new(
                "evo_agent_llm_bypass_audit_total",
                "LLM calls bypassing the audit chain (shadow calls), by purpose",
            ),
            &["purpose"],
        )
        .map_err(|_| {
            MetricsError::CounterCreationFailed("evo_agent_llm_bypass_audit_total".into())
        })?;
        let safety_audit_hits_total = IntCounterVec::new(
            Opts::new(
                "evo_agent_safety_audit_hits_total",
                "L2 SafetyAuditor hits on recalled memory content, by rule",
            ),
            &["rule"],
        )
        .map_err(|_| {
            MetricsError::CounterCreationFailed("evo_agent_safety_audit_hits_total".into())
        })?;
        let memory_cache_drift_total = IntCounter::new(
            "evo_agent_memory_cache_drift_total",
            "Memory cache entries drifted from evorule source of truth (B3)",
        )
        .map_err(|_| {
            MetricsError::CounterCreationFailed("evo_agent_memory_cache_drift_total".into())
        })?;

        registry
            .register(Box::new(sessions_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_sessions_total".into())
            })?;
        registry
            .register(Box::new(sessions_active.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_sessions_active".into())
            })?;
        registry
            .register(Box::new(llm_calls_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_llm_calls_total".into())
            })?;
        registry
            .register(Box::new(llm_duration_seconds.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_llm_duration_seconds".into())
            })?;
        registry
            .register(Box::new(tool_calls_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_tool_calls_total".into())
            })?;
        registry
            .register(Box::new(tool_duration_seconds.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_tool_duration_seconds".into())
            })?;
        registry
            .register(Box::new(steps_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_steps_total".into())
            })?;
        registry
            .register(Box::new(sse_connections.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_sse_connections".into())
            })?;
        registry
            .register(Box::new(llm_bypass_audit_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_llm_bypass_audit_total".into())
            })?;
        registry
            .register(Box::new(safety_audit_hits_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_safety_audit_hits_total".into())
            })?;
        registry
            .register(Box::new(memory_cache_drift_total.clone()))
            .map_err(|_| {
                MetricsError::RegistryRegistrationFailed("evo_agent_memory_cache_drift_total".into())
            })?;

        Ok(Self {
            registry,
            sessions_total,
            sessions_active,
            llm_calls_total,
            llm_duration_seconds,
            tool_calls_total,
            tool_duration_seconds,
            steps_total,
            sse_connections,
            llm_bypass_audit_total,
            safety_audit_hits_total,
            memory_cache_drift_total,
        })
    }

    /// 渲染所有指标为 Prometheus 文本格式(供 `/metrics` 端点返回)
    pub fn render(&self) -> String {
        let encoder = TextEncoder::new();
        let mfs = self.registry.gather();
        encoder
            .encode_to_string(&mfs)
            .unwrap_or_else(|e| format!("# encoding error: {e}"))
    }

    /// session 创建总数 +1
    pub fn inc_sessions_total(&self) {
        self.sessions_total.inc();
    }

    /// 活跃 session +1
    pub fn inc_sessions_active(&self) {
        self.sessions_active.inc();
    }

    /// 活跃 session -1
    pub fn dec_sessions_active(&self) {
        self.sessions_active.dec();
    }

    /// 步数 +1
    pub fn inc_steps(&self) {
        self.steps_total.inc();
    }

    /// 记录 LLM 调用(ok/error 计数 + 耗时直方图)
    pub fn observe_llm_call(&self, model: &str, duration: Duration, ok: bool) {
        let status = if ok { "ok" } else { "error" };
        self.llm_calls_total.with_label_values(&[status]).inc();
        self.llm_duration_seconds
            .with_label_values(&[model])
            .observe(duration.as_secs_f64());
    }

    /// 仅计数 LLM 调用(不记录耗时直方图,供 G18 MetricsCallback 使用)
    ///
    /// 与 [`observe_llm_call`] 的区别:不需要 model/duration 参数,
    /// 只递增 `llm_calls_total` counter。适用于从 `AgentEvent` 推断调用结果的场景。
    pub fn inc_llm_calls(&self, ok: bool) {
        let status = if ok { "ok" } else { "error" };
        self.llm_calls_total.with_label_values(&[status]).inc();
    }

    /// 记录工具调用(ok/error 计数 + 耗时直方图)
    pub fn observe_tool_call(&self, tool: &str, duration: Duration, ok: bool) {
        let status = if ok { "ok" } else { "error" };
        self.tool_calls_total
            .with_label_values(&[tool, status])
            .inc();
        self.tool_duration_seconds
            .with_label_values(&[tool])
            .observe(duration.as_secs_f64());
    }

    /// 仅计数工具调用(不记录耗时直方图,供 G18 MetricsCallback 使用)
    ///
    /// 与 [`observe_tool_call`] 的区别:不需要 duration 参数,
    /// 只递增 `tool_calls_total` counter。适用于从 `AgentEvent::ToolResult` 推断结果的场景。
    pub fn inc_tool_calls(&self, tool: &str, ok: bool) {
        let status = if ok { "ok" } else { "error" };
        self.tool_calls_total
            .with_label_values(&[tool, status])
            .inc();
    }

    /// SSE 连接 +1
    pub fn inc_sse_connections(&self) {
        self.sse_connections.inc();
    }

    /// SSE 连接 -1
    pub fn dec_sse_connections(&self) {
        self.sse_connections.dec();
    }

    /// 绕过审计链的 LLM 影子调用计数 +1（P2-V3 止血，2026-08-27）
    ///
    /// - `purpose`：调用用途（如 "summarize"、"rollup"、"session_summary"）
    ///
    /// Summarizer 直连 LLM 不经 evorule fact 流程，本指标使这一审计盲区
    /// 的大小可量化、可告警。
    pub fn inc_llm_bypass_audit(&self, purpose: &str) {
        self.llm_bypass_audit_total
            .with_label_values(&[purpose])
            .inc();
    }

    /// L2 SafetyAuditor 命中 +1（P5-A3 指标，按命中规则分桶）
    pub fn inc_safety_audit_hit(&self, rule: &str) {
        self.safety_audit_hits_total.with_label_values(&[rule]).inc();
    }

    /// B3：memory cache 漂移条目 +n（定期校验对齐时累计）
    pub fn inc_memory_cache_drift(&self, n: u64) {
        self.memory_cache_drift_total.inc_by(n);
    }
}

/// 共享指标引用(Arc 包装,供 handler 和 runner 共享)
pub type SharedMetrics = Arc<Metrics>;

/// 活跃 session RAII 守卫 — 创建时 inc,drop 时 dec
///
/// 放在 `run()` / `run_streaming()` 中 session 创建之后,
/// 确保所有返回路径(包括 error / cancel)都正确 dec。
pub struct SessionActiveGuard {
    metrics: Option<SharedMetrics>,
}

impl SessionActiveGuard {
    /// 创建守卫并 inc 活跃 session 计数
    pub fn new(metrics: Option<SharedMetrics>) -> Self {
        if let Some(m) = &metrics {
            m.inc_sessions_active();
        }
        Self { metrics }
    }
}

impl Drop for SessionActiveGuard {
    fn drop(&mut self) {
        if let Some(m) = &self.metrics {
            m.dec_sessions_active();
        }
    }
}

/// SSE 连接 RAII 守卫 — 创建时 inc,drop 时 dec
///
/// 放在 SSE stream 生成器中,确保流结束(正常关闭/客户端断开/error)时 dec。
pub struct SseConnectionGuard {
    metrics: SharedMetrics,
}

impl SseConnectionGuard {
    /// 创建守卫并 inc SSE 连接计数
    pub fn new(metrics: SharedMetrics) -> Self {
        metrics.inc_sse_connections();
        Self { metrics }
    }
}

impl Drop for SseConnectionGuard {
    fn drop(&mut self) {
        self.metrics.dec_sse_connections();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn make_metrics() -> Metrics {
        Metrics::new().unwrap()
    }

    #[test]
    fn test_new_registers_all_metrics() {
        let m = make_metrics();
        // Vec 指标(IntCounterVec / HistogramVec)在 Prometheus 0.13 中是惰性创建子项的,
        // 未调用 with_label_values 前不会出现在 render() 输出中。
        // 这里调用一次 observe 以初始化所有 Vec 子项,验证它们都能正确注册。
        m.observe_llm_call("test_model", Duration::from_millis(1), true);
        m.observe_tool_call("test_tool", Duration::from_millis(1), true);
        let output = m.render();
        assert!(output.contains("evo_agent_sessions_total"));
        assert!(output.contains("evo_agent_sessions_active"));
        assert!(output.contains("evo_agent_llm_calls_total"));
        assert!(output.contains("evo_agent_llm_duration_seconds"));
        assert!(output.contains("evo_agent_tool_calls_total"));
        assert!(output.contains("evo_agent_tool_duration_seconds"));
        assert!(output.contains("evo_agent_steps_total"));
        assert!(output.contains("evo_agent_sse_connections"));
    }

    #[test]
    fn test_sessions_total_counter() {
        let m = make_metrics();
        m.inc_sessions_total();
        m.inc_sessions_total();
        m.inc_sessions_total();
        let output = m.render();
        assert!(output.contains("evo_agent_sessions_total 3"));
    }

    #[test]
    fn test_sessions_active_gauge_inc_dec() {
        let m = make_metrics();
        m.inc_sessions_active();
        m.inc_sessions_active();
        m.dec_sessions_active();
        let output = m.render();
        assert!(output.contains("evo_agent_sessions_active 1"));
    }

    #[test]
    fn test_steps_counter() {
        let m = make_metrics();
        for _ in 0..5 {
            m.inc_steps();
        }
        let output = m.render();
        assert!(output.contains("evo_agent_steps_total 5"));
    }

    #[test]
    fn test_llm_calls_by_status() {
        let m = make_metrics();
        m.observe_llm_call("gpt-4o", Duration::from_millis(500), true);
        m.observe_llm_call("gpt-4o", Duration::from_millis(1200), true);
        m.observe_llm_call("gpt-4o", Duration::from_millis(300), false);
        let output = m.render();
        assert!(output.contains("evo_agent_llm_calls_total{status=\"ok\"} 2"));
        assert!(output.contains("evo_agent_llm_calls_total{status=\"error\"} 1"));
    }

    /// P2-V3 止血 + P5-A3 指标：旁路调用按 purpose 分桶、L2 审计命中按 rule 分桶
    #[test]
    fn test_bypass_and_safety_audit_counters() {
        let m = make_metrics();
        m.inc_llm_bypass_audit("summarize");
        m.inc_llm_bypass_audit("summarize");
        m.inc_llm_bypass_audit("rollup");
        m.inc_safety_audit_hit("instruction_override");
        let output = m.render();
        assert!(output.contains(
            "evo_agent_llm_bypass_audit_total{purpose=\"summarize\"} 2"
        ));
        assert!(output.contains("evo_agent_llm_bypass_audit_total{purpose=\"rollup\"} 1"));
        assert!(output
            .contains("evo_agent_safety_audit_hits_total{rule=\"instruction_override\"} 1"));
    }

    #[test]
    fn test_llm_duration_histogram_by_model() {
        let m = make_metrics();
        m.observe_llm_call("gpt-4o", Duration::from_millis(500), true);
        m.observe_llm_call("deepseek-v3", Duration::from_millis(200), true);
        let output = m.render();
        assert!(output.contains("evo_agent_llm_duration_seconds_bucket"));
        assert!(output.contains("evo_agent_llm_duration_seconds_count"));
        assert!(output.contains("evo_agent_llm_duration_seconds_sum"));
        assert!(output.contains("model=\"gpt-4o\""));
        assert!(output.contains("model=\"deepseek-v3\""));
    }

    #[test]
    fn test_tool_calls_by_tool_and_status() {
        let m = make_metrics();
        m.observe_tool_call("file_read", Duration::from_millis(10), true);
        m.observe_tool_call("file_read", Duration::from_millis(5), true);
        m.observe_tool_call("shell_exec", Duration::from_millis(100), false);
        let output = m.render();
        // Prometheus 0.13 按 label 名字母序输出:status < tool
        assert!(output.contains("evo_agent_tool_calls_total{status=\"ok\",tool=\"file_read\"} 2"));
        assert!(
            output.contains("evo_agent_tool_calls_total{status=\"error\",tool=\"shell_exec\"} 1")
        );
    }

    #[test]
    fn test_tool_duration_histogram() {
        let m = make_metrics();
        m.observe_tool_call("file_read", Duration::from_millis(50), true);
        let output = m.render();
        assert!(output.contains("evo_agent_tool_duration_seconds_bucket"));
        assert!(output.contains("tool=\"file_read\""));
    }

    #[test]
    fn test_sse_connections_gauge() {
        let m = make_metrics();
        m.inc_sse_connections();
        m.inc_sse_connections();
        m.dec_sse_connections();
        let output = m.render();
        assert!(output.contains("evo_agent_sse_connections 1"));
    }

    #[test]
    fn test_session_active_guard_inc_dec() {
        let m = Arc::new(make_metrics());
        {
            let _guard = SessionActiveGuard::new(Some(m.clone()));
            let output = m.render();
            assert!(output.contains("evo_agent_sessions_active 1"));
        }
        // guard dropped
        let output = m.render();
        assert!(output.contains("evo_agent_sessions_active 0"));
    }

    #[test]
    fn test_session_active_guard_none_is_noop() {
        let _guard = SessionActiveGuard::new(None);
        // Should not panic
    }

    #[test]
    fn test_sse_connection_guard_inc_dec() {
        let m = Arc::new(make_metrics());
        {
            let _guard = SseConnectionGuard::new(m.clone());
            let output = m.render();
            assert!(output.contains("evo_agent_sse_connections 1"));
        }
        let output = m.render();
        assert!(output.contains("evo_agent_sse_connections 0"));
    }

    #[test]
    fn test_custom_registry_isolation() {
        // 两个独立的 Metrics 实例互不干扰(自定义 Registry vs 全局)
        let m1 = Arc::new(make_metrics());
        let m2 = Arc::new(make_metrics());

        m1.inc_sessions_total();
        m1.inc_steps();

        let out1 = m1.render();
        let out2 = m2.render();

        assert!(out1.contains("evo_agent_sessions_total 1"));
        assert!(out1.contains("evo_agent_steps_total 1"));
        // m2 不受影响
        assert!(out2.contains("evo_agent_sessions_total 0"));
        assert!(out2.contains("evo_agent_steps_total 0"));
    }
}
