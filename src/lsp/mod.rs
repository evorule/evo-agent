// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LSP 代理模块:语言服务器长驻子进程 + stdio JSON-RPC 传输 + WS 透明转发。
//!
//! 形态定位:serve 是**纯透明转发**代理(不实现 LSP server/client 语义),
//! 仅两个出向拦截点(日志通道分流 / publishDiagnostics 附加工作区相对路径),
//! 能力协商、server 请求、厂商扩展全保真穿透。生命周期治理:
//!
//! - 每语言**单连接单实例**(注册表占用中拒绝第二连接);
//! - 进程意外退出按**退避重启**处理:60 秒窗口内最多 3 次(1s/2s/4s),
//!   超限进 Failed 终态(前端清诊断+横幅+手动重启命令);
//! - 客户端发出 shutdown 后的进程退出视为正常停止,不重启;
//! - WS 断开即 kill 子进程(孤儿防护)。

#![forbid(unsafe_code)]

pub mod bridge;
pub mod process;
pub mod transport;

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::api::agent_api::AgentApiState;

/// 语言门(v1 两语言;PATH 硬发现,缺失由上层灰置降级)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    /// Rust 门:rust-analyzer
    Rust,
    /// TypeScript 门:typescript-language-server(兼管 javascript)
    TypeScript,
}

impl Lang {
    /// WS query 参数形态(`rust` | `typescript`;javascript 归入 typescript 门,
    /// 同由 typescript-language-server 服务)。
    pub fn from_query(value: &str) -> Option<Self> {
        match value {
            "rust" => Some(Self::Rust),
            "typescript" => Some(Self::TypeScript),
            _ => None,
        }
    }

    /// WS query 参数与日志面使用的规范名
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::TypeScript => "typescript",
        }
    }

    /// 对应语言门的启动规格
    pub fn spec(&self) -> process::ServerSpec {
        match self {
            Self::Rust => process::RUST_ANALYZER,
            Self::TypeScript => process::TYPESCRIPT_SERVER,
        }
    }

    /// 诊断 owner 命名空间(诊断单源契约:接入方自报,全局唯一)。
    /// 诊断 owner 命名空间(诊断单源契约:接入方自报,全局唯一)。
    pub fn owner(&self) -> &'static str {
        match self {
            // rust-analyzer 以内嵌点命名;typescript-language-server 以连字符
            Self::Rust => "lsp.rust-analyzer",
            Self::TypeScript => "lsp.typescript-language-server",
        }
    }
}

/// 重启退避策略:60 秒窗口内最多 3 次重启,退避 1s/2s/4s,超限终态。
#[derive(Debug)]
pub struct RestartPolicy {
    window: Duration,
    max_restarts: u32,
    exit_times: Vec<Instant>,
}

/// 单次进程退出的处置决定。
#[derive(Debug, PartialEq, Eq)]
pub enum RestartDecision {
    /// 按给定退避时长重启
    Retry(Duration),
    /// 窗口内重启次数耗尽,进 Failed
    Exhausted,
}

impl RestartPolicy {
    /// 生产参数:60 秒窗口,最多 3 次,退避 1s/2s/4s。
    pub fn new() -> Self {
        Self {
            window: Duration::from_secs(60),
            max_restarts: 3,
            exit_times: Vec::new(),
        }
    }

    /// 记录一次意外退出,返回重启或终态决定。
    pub fn record_exit(&mut self, now: Instant) -> RestartDecision {
        let cutoff = now - self.window;
        self.exit_times.retain(|t| *t >= cutoff);
        if self.exit_times.len() >= self.max_restarts as usize {
            return RestartDecision::Exhausted;
        }
        let backoff = Duration::from_secs(1 << self.exit_times.len().min(2)); // 1s/2s/4s
        self.exit_times.push(now);
        RestartDecision::Retry(backoff)
    }

    /// 窗口内已登记的重启次数(状态展示用)。
    pub fn restarts_in_window(&self) -> usize {
        self.exit_times.len()
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// 每语言活动连接注册表(单连接单实例;占用中拒绝新连接)。
///
/// 挂在 [`AgentApiState`] 上(跨 WS 连接共享);连接建立 insert、任何退出路径
/// remove——remove 幂等,handler 与 bridge 双侧调用安全。
#[derive(Debug, Default)]
pub struct LangRegistry {
    active: std::sync::Mutex<HashSet<&'static str>>,
}

impl LangRegistry {
    /// 空注册表。
    pub fn new() -> Self {
        Self::default()
    }

    /// 占用语言槽;已在用返回 false(调用方拒连接)。
    pub fn try_acquire(&self, lang: Lang) -> bool {
        let mut set = self.active.lock().unwrap_or_else(|e| e.into_inner());
        set.insert(lang.as_str())
    }

    /// 释放语言槽(幂等)。
    pub fn release(&self, lang: Lang) {
        let mut set = self.active.lock().unwrap_or_else(|e| e.into_inner());
        set.remove(lang.as_str());
    }
}

/// 取 state 上的共享注册表引用。
pub fn registry(state: &AgentApiState) -> &LangRegistry {
    state.lsp_registry()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy_with_window(secs: u64) -> RestartPolicy {
        // 测试用小窗口缩短等待;窗口/上限语义与生产一致
        RestartPolicy {
            window: Duration::from_secs(secs),
            max_restarts: 3,
            exit_times: Vec::new(),
        }
    }

    #[test]
    fn first_three_exits_retry_then_exhausted() {
        let mut p = policy_with_window(60);
        let t = Instant::now();
        assert_eq!(
            p.record_exit(t),
            RestartDecision::Retry(Duration::from_secs(1))
        );
        assert_eq!(
            p.record_exit(t),
            RestartDecision::Retry(Duration::from_secs(2))
        );
        assert_eq!(
            p.record_exit(t),
            RestartDecision::Retry(Duration::from_secs(4))
        );
        assert_eq!(p.record_exit(t), RestartDecision::Exhausted);
    }

    #[test]
    fn old_exits_expire_from_window() {
        let mut p = policy_with_window(1); // 1 秒窗口
        let t = Instant::now();
        assert_eq!(
            p.record_exit(t),
            RestartDecision::Retry(Duration::from_secs(1))
        );
        assert_eq!(
            p.record_exit(t),
            RestartDecision::Retry(Duration::from_secs(2))
        );
        // 窗口滑过后旧退出不再计数:重新从头计
        std::thread::sleep(Duration::from_millis(1100));
        let t2 = Instant::now();
        assert_eq!(
            p.record_exit(t2),
            RestartDecision::Retry(Duration::from_secs(1))
        );
    }

    #[test]
    fn lang_query_and_owner() {
        assert_eq!(Lang::from_query("rust"), Some(Lang::Rust));
        assert_eq!(Lang::from_query("typescript"), Some(Lang::TypeScript));
        assert_eq!(Lang::from_query("python"), None);
        assert_eq!(Lang::Rust.owner(), "lsp.rust-analyzer");
        assert_eq!(Lang::TypeScript.owner(), "lsp.typescript-language-server");
        assert_eq!(Lang::TypeScript.spec().args, ["--stdio"]);
    }

    #[test]
    fn registry_single_instance_per_lang() {
        let reg = LangRegistry::new();
        assert!(reg.try_acquire(Lang::Rust));
        assert!(!reg.try_acquire(Lang::Rust)); // 占用中
        assert!(reg.try_acquire(Lang::TypeScript)); // 不同语言互不干扰
        reg.release(Lang::Rust);
        assert!(reg.try_acquire(Lang::Rust));
        reg.release(Lang::Rust); // 幂等
    }
}
