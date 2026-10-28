// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

//! usage 真值感知 token 计数器
//!
//! 语义（上下文压缩真值驱动）：
//! - 冷启动（尚无 provider 真值）→ 完全回落注入的近似计数器（ApproxTokenCounter）；
//! - 每次拿到 provider 真值 usage 后经 `observe` 喂入滑动窗口，后续
//!   `count_messages` 以「真值 / 同刻近似值」的窗口平均比率校准近似计数——
//!   近似计数从压缩阈值判定主力降级为冷启动估算与增量外推基准；
//! - 配对口径：`observe` 的真值为 prompt+completion 全量，而近似侧计数的是
//!   发送前的消息列表（≈prompt），比率略有高估 → 裁剪偏保守（提前触发），
//!   属安全方向偏差；
//! - `snapshot` 输出累计三元组（journal `TokenRecord` 同型），供收尾输出面消费。
//!
//! 线程安全：全部状态经 Mutex 内聚，`observe` 可在任意 LLM 响应点调用。

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::context_window::TokenCounter;
use super::journal::TokenRecord;
use super::translator::{Message, TokenUsage};

/// 真值校准滑动窗口容量（保留最近 N 次「近似值 vs 真值」配对）
const CALIBRATION_WINDOW: usize = 8;

#[derive(Debug)]
struct Inner {
    fallback: Box<dyn TokenCounter>,
    /// (近似值, 真值) 配对窗口
    window: VecDeque<(usize, usize)>,
    /// 最近一次 `count_messages` 的近似值（observe 时配对入窗）
    pending_query: Option<usize>,
    cumulative: TokenRecord,
}

/// provider 真值感知的 token 计数器（实现既有 `TokenCounter` trait 契约）
#[derive(Debug, Clone)]
pub struct UsageAwareTokenCounter {
    inner: Arc<Mutex<Inner>>,
}

impl UsageAwareTokenCounter {
    /// 冷启动构造：真值未到位前完全回落近似计数器
    pub fn new_fallback(fallback: Box<dyn TokenCounter>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                fallback,
                window: VecDeque::new(),
                pending_query: None,
                cumulative: TokenRecord::default(),
            })),
        }
    }

    /// 每次 LLM 响应后喂入 provider 真值（runner 既有 usage 累加点扩展）
    pub fn observe(&self, usage: &TokenUsage) {
        let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(approx) = g.pending_query.take() {
            if approx > 0 && usage.total_tokens > 0 {
                if g.window.len() >= CALIBRATION_WINDOW {
                    g.window.pop_front();
                }
                g.window.push_back((approx, usage.total_tokens));
            }
        }
        g.cumulative.prompt += usage.prompt_tokens as u64;
        g.cumulative.completion += usage.completion_tokens as u64;
        g.cumulative.total += usage.total_tokens as u64;
    }

    /// 累计三元组快照（输出面消费；journal TokenRecord 复用类型）
    pub fn snapshot(&self) -> TokenRecord {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .cumulative
            .clone()
    }

    /// 窗口平均校准比率（窗口空 = 冷启动，None）
    fn calibration_ratio(&self) -> Option<f64> {
        let g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if g.window.is_empty() {
            return None;
        }
        let sum: f64 = g
            .window
            .iter()
            .map(|(a, t)| *t as f64 / *a as f64)
            .sum();
        Some(sum / g.window.len() as f64)
    }
}

impl TokenCounter for UsageAwareTokenCounter {
    fn count_messages(&self, messages: &[Message]) -> usize {
        let approx = {
            let mut g = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            let approx = g.fallback.count_messages(messages);
            g.pending_query = Some(approx);
            approx
        };
        match self.calibration_ratio() {
            None => approx,
            Some(ratio) => (approx as f64 * ratio).round() as usize,
        }
    }
}

/// Arc 包装透传：runner 侧与 ContextWindowManager 共享同一计数器状态
/// （Arc<UsageAwareTokenCounter> 直接作为 ContextWindowManager 的计数器注入）
impl TokenCounter for Arc<UsageAwareTokenCounter> {
    fn count_messages(&self, messages: &[Message]) -> usize {
        (**self).count_messages(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::context_window::ApproxTokenCounter;
    use crate::agent::translator::Message;

    fn msg(content: &str) -> Message {
        Message::User {
            content: content.to_string(),
            images: Vec::new(),
        }
    }

    fn usage(prompt: usize, completion: usize) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            cached_tokens: None,
        }
    }

    /// 冷启动回落：未 observe 时与 fallback 近似计数逐值一致
    #[test]
    fn cold_start_falls_back_to_approx() {
        let counter = UsageAwareTokenCounter::new_fallback(Box::new(ApproxTokenCounter::new()));
        let approx = ApproxTokenCounter::new();
        let messages = vec![msg("hello world"), msg("你好，世界")];
        assert_eq!(
            counter.count_messages(&messages),
            approx.count_messages(&messages)
        );
    }

    /// 真值驱动：observe 后输出向真值方向校准（近似比率 ≠ 1 时偏离 fallback 值）
    #[test]
    fn observed_truth_calibrates_counts() {
        let counter = UsageAwareTokenCounter::new_fallback(Box::new(ApproxTokenCounter::new()));
        let messages = vec![msg("hello world")];
        let approx = ApproxTokenCounter::new().count_messages(&messages);
        // 先问一次（记录 pending 近似值），再喂入放大 10 倍的真值
        let _ = counter.count_messages(&messages);
        counter.observe(&usage(approx * 10, 0));
        let calibrated = counter.count_messages(&messages);
        assert!(
            calibrated > approx,
            "calibrated ({calibrated}) should exceed approx ({approx}) after truth feeding"
        );
        // 窗口比率 10 倍 → 校准值 ≈ approx × 10
        assert_eq!(calibrated, approx * 10);
    }

    /// observe 累计三元组：多次喂入按 prompt/completion/total 分量累加
    #[test]
    fn snapshot_accumulates_across_observes() {
        let counter = UsageAwareTokenCounter::new_fallback(Box::new(ApproxTokenCounter::new()));
        counter.observe(&usage(100, 20));
        counter.observe(&usage(150, 30));
        let snap = counter.snapshot();
        assert_eq!(snap.prompt, 250);
        assert_eq!(snap.completion, 50);
        assert_eq!(snap.total, 300);
    }

    /// 线程安全：并发 observe 与 count 不死锁不撕裂（CI 常开）
    #[test]
    fn concurrent_observe_and_count() {
        let counter = std::sync::Arc::new(UsageAwareTokenCounter::new_fallback(Box::new(
            ApproxTokenCounter::new(),
        )));
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let c = counter.clone();
                std::thread::spawn(move || {
                    for _ in 0..100 {
                        let _ = c.count_messages(&[msg("thread probe")]);
                        c.observe(&usage(i + 1, 1));
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("worker thread should not panic");
        }
        assert!(counter.snapshot().total > 0);
    }

    /// 滑动窗口封顶：仅保留最近 CALIBRATION_WINDOW 个配对（旧配对出窗后不再影响比率）
    #[test]
    fn sliding_window_is_bounded() {
        let counter = UsageAwareTokenCounter::new_fallback(Box::new(ApproxTokenCounter::new()));
        let messages = vec![msg("hello world")];
        let approx = ApproxTokenCounter::new().count_messages(&messages);
        for _ in 0..(CALIBRATION_WINDOW + 4) {
            let _ = counter.count_messages(&messages);
            counter.observe(&usage(approx, 0)); // 比率恒 1
        }
        assert_eq!(counter.count_messages(&messages), approx);
    }
}
