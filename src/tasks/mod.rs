// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 任务运行管理:run_id→运行记录(回放缓冲+活流广播+取消编排);并发上限 4。
//!
//! 事件流形态(WS 下行 JSON 字符串,预序列化):
//! `started` / `output` / `diagnostics` / `ended` / `gov_event`。
//! 每 run 保留有界回放缓冲(2000 事件)——WS 断连重连先回放后续播;
//! ended 后保留最近若干 run 供状态查询,超量淘汰最旧。

#![forbid(unsafe_code)]

pub mod matcher;
pub mod runner;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

/// 并发任务上限
pub const MAX_CONCURRENT: usize = 4;
/// 每 run 回放缓冲上限(事件条数)
pub const REPLAY_CAP: usize = 2000;
/// 结束态 run 保留上限(超量淘汰最旧)
const FINISHED_KEEP: usize = 20;

/// 一次任务运行的全部共享面(Arc 分发;终态字段原子化免锁)
#[derive(Debug)]
pub struct TaskRun {
    /// 任务 label
    pub label: String,
    /// 命令原文(确认框/治理留痕)
    pub command_display: String,
    /// 进程 id(kill/日志)
    pub pid: Option<u32>,
    /// 取消编排通道(orchestrator 消费:phase1 → 窗口 → phase2)
    pub cancel_tx: mpsc::Sender<u32>,
    finished: AtomicBool,
    exit_code: AtomicI32,
    cancelled: AtomicBool,
    /// 事件回放缓冲 + 活流(预序列化 JSON 行)
    replay: Mutex<VecDeque<String>>,
    broadcast: tokio::sync::broadcast::Sender<String>,
}

impl TaskRun {
    /// 推送事件:入回放缓冲(超限头删)+广播给全部订阅者
    pub fn emit(&self, event_json: String) {
        {
            let mut buf = self.replay.lock().unwrap_or_else(|e| e.into_inner());
            if buf.len() >= REPLAY_CAP {
                buf.pop_front();
            }
            buf.push_back(event_json.clone());
        }
        // 无订阅者错误属正常:前端可能尚未连 WS
        let _ = self.broadcast.send(event_json);
    }

    /// 标记终态(幂等;恰在编排任务收尾时调用一次)
    pub fn finish(&self, exit_code: i32, cancelled: bool) {
        self.exit_code.store(exit_code, Ordering::SeqCst);
        self.cancelled.store(cancelled, Ordering::SeqCst);
        self.finished.store(true, Ordering::SeqCst);
    }

    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> Option<i32> {
        if self.is_finished() {
            Some(self.exit_code.load(Ordering::SeqCst))
        } else {
            None
        }
    }

    pub fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// 订阅活播流(回放见 [`TaskRun::replay_snapshot`])
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<String> {
        self.broadcast.subscribe()
    }

    /// 回放快照(克隆出锁;MutexGuard 不可跨 await)
    pub fn replay_snapshot(&self) -> Vec<String> {
        let buf = self.replay.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter().cloned().collect()
    }
}

/// 运行注册表(挂 AgentApiState 共享;Arc 分发使读取免锁持有)
#[derive(Debug, Default)]
pub struct TaskManager {
    runs: Mutex<HashMap<String, Arc<TaskRun>>>,
    counter: Mutex<u64>,
}

impl TaskManager {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一次运行;超量时淘汰最旧的结束态 run
    pub fn register(
        &self,
        label: &str,
        command_display: &str,
        pid: Option<u32>,
        cancel_tx: mpsc::Sender<u32>,
        broadcast: tokio::sync::broadcast::Sender<String>,
    ) -> String {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let finished: Vec<String> = runs
            .iter()
            .filter(|(_, r)| r.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        if finished.len() >= FINISHED_KEEP {
            let mut keys = finished;
            keys.sort();
            if let Some(oldest) = keys.first() {
                runs.remove(oldest);
            }
        }
        let mut counter = self.counter.lock().unwrap_or_else(|e| e.into_inner());
        *counter += 1;
        let run_id = format!("run-{}", *counter);
        runs.insert(
            run_id.clone(),
            Arc::new(TaskRun {
                label: label.to_string(),
                command_display: command_display.to_string(),
                pid,
                cancel_tx,
                finished: AtomicBool::new(false),
                exit_code: AtomicI32::new(-1),
                cancelled: AtomicBool::new(false),
                replay: Mutex::new(VecDeque::new()),
                broadcast,
            }),
        );
        run_id
    }

    /// 取运行记录共享句柄。
    pub fn get(&self, run_id: &str) -> Option<Arc<TaskRun>> {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(run_id)
            .cloned()
    }

    /// 同 label 是否有未结束运行
    pub fn is_label_running(&self, label: &str) -> bool {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .any(|r| r.label == label && !r.is_finished())
    }

    /// 全部活动(未结束)run 的快照
    pub fn active_summaries(&self) -> Vec<(String, String)> {
        self.runs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, r)| !r.is_finished())
            .map(|(k, r)| (k.clone(), r.label.clone()))
            .collect()
    }

    /// 取消某 run(两段式第一段);返回是否已递送
    pub fn cancel(&self, run_id: &str) -> Option<bool> {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let run = runs.get(run_id)?;
        if run.is_finished() {
            return Some(false);
        }
        run.cancel_tx.try_send(1).ok()?;
        Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn register_emit_subscribe_replay() {
        let mgr = TaskManager::new();
        let (cancel_tx, _cancel_rx) = mpsc::channel(4);
        let (btx, _brx) = tokio::sync::broadcast::channel::<String>(1024);
        let run_id = mgr.register("build", "cargo build", Some(42), cancel_tx, btx);
        let run = mgr.get(&run_id).unwrap();
        run.emit(r#"{"type":"started"}"#.into());
        run.emit(r#"{"type":"output"}"#.into());
        // 后订阅者:回放缓冲拿全量;终态原子面
        assert_eq!(run.replay_snapshot().len(), 2);
        assert!(run.replay_snapshot()[0].contains("started"));
        assert!(!run.is_finished());
        run.finish(0, false);
        assert_eq!(run.exit_code(), Some(0));
        assert!(mgr.get(&run_id).unwrap().is_finished());
    }

    #[test]
    fn label_running_guard() {
        let mgr = TaskManager::new();
        let (cancel_tx, _cancel_rx) = mpsc::channel(4);
        let (btx, _brx) = tokio::sync::broadcast::channel::<String>(16);
        let id = mgr.register("build", "cargo build", None, cancel_tx, btx);
        assert!(mgr.is_label_running("build"));
        mgr.get(&id).unwrap().finish(0, false);
        assert!(!mgr.is_label_running("build"));
    }
}
