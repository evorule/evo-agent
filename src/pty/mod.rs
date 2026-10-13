// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! PTY 会话模块:工作台集成终端的长驻伪终端管理。
//!
//! 定位:**人的终端**(与 agent 的命令执行通道互不相通——前者零拦截仅会话级
//! 留痕,后者走命令执行工具的三层审批)。形态要点:
//!
//! - portable-pty 同步 IO(ConPTY 管道无异步语义)→ 专用线程桥入 tokio;
//! - 每终端单 WS 连接;并发上限 8;WS 断开/退出必杀进程树(防孤儿);
//! - 治理分级:会话创建/关闭=治理事件(record-only,展示层),会话内零拦截。

#![forbid(unsafe_code)]

pub mod session;

use std::sync::Mutex;

/// 并发终端上限(内存与进程数护栏)。
pub const MAX_SESSIONS: usize = 8;

/// 活动会话计数护栏(跨 WS 连接共享;acquire/release 幂等配对由调用方保证)。
#[derive(Debug, Default)]
pub struct PtyRegistry {
    active: Mutex<usize>,
}

impl PtyRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 占用一个终端槽;已达上限返回 false(调用方拒绝连接)。
    pub fn try_acquire(&self) -> bool {
        let mut n = self.active.lock().unwrap_or_else(|e| e.into_inner());
        if *n >= MAX_SESSIONS {
            return false;
        }
        *n += 1;
        true
    }

    /// 释放终端槽(下限钳零;release 与 acquire 必须配对)。
    pub fn release(&self) {
        let mut n = self.active.lock().unwrap_or_else(|e| e.into_inner());
        *n = n.saturating_sub(1);
    }

    /// 当前活动数(状态面/测试用)。
    pub fn active(&self) -> usize {
        *self.active.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 上行控制帧白名单解析(仅 `resize`|`pause`|`resume`,未知帧丢弃)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyControl {
    /// 终端尺寸变更
    Resize {
        /// 列数
        cols: u16,
        /// 行数
        rows: u16,
    },
    /// 前端写队列超水位,暂停转发
    Pause,
    /// 前端写队列回落,恢复转发
    Resume,
}

/// 上行控制帧解析(白名单外返回 None,调用方丢弃)。
pub fn parse_control_frame(text: &str) -> Option<PtyControl> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    match value.get("type")?.as_str()? {
        "resize" => {
            let cols = value.get("cols")?.as_u64()?;
            let rows = value.get("rows")?.as_u64()?;
            if cols == 0 || rows == 0 || cols > 500 || rows > 300 {
                return None; // ConPTY 零尺寸坑+护栏
            }
            Some(PtyControl::Resize {
                cols: cols as u16,
                rows: rows as u16,
            })
        }
        "pause" => Some(PtyControl::Pause),
        "resume" => Some(PtyControl::Resume),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_cap_eight_then_deny() {
        let reg = PtyRegistry::new();
        for _ in 0..MAX_SESSIONS {
            assert!(reg.try_acquire());
        }
        assert!(!reg.try_acquire()); // 第 9 个拒绝
        assert_eq!(reg.active(), MAX_SESSIONS);
        reg.release();
        assert!(reg.try_acquire());
    }

    #[test]
    fn release_clamps_at_zero() {
        let reg = PtyRegistry::new();
        reg.release();
        reg.release();
        assert_eq!(reg.active(), 0);
        assert!(reg.try_acquire());
    }

    #[test]
    fn control_frame_whitelist() {
        assert_eq!(
            parse_control_frame(r#"{"type":"resize","cols":120,"rows":30}"#),
            Some(PtyControl::Resize {
                cols: 120,
                rows: 30
            })
        );
        assert_eq!(
            parse_control_frame(r#"{"type":"pause"}"#),
            Some(PtyControl::Pause)
        );
        assert_eq!(
            parse_control_frame(r#"{"type":"resume"}"#),
            Some(PtyControl::Resume)
        );
        // 白名单外/畸形/危险尺寸一律丢弃
        assert_eq!(parse_control_frame(r#"{"type":"kill"}"#), None);
        assert_eq!(
            parse_control_frame(r#"{"type":"resize","cols":0,"rows":24}"#),
            None
        );
        assert_eq!(parse_control_frame(r#"{"type":"resize"}"#), None);
        assert_eq!(parse_control_frame("not-json"), None);
    }
}
