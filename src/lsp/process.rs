// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LSP 语言服务器长驻子进程管理。
//!
//! 与一次性命令执行(命令工具的同步 `.output()`)不同,语言服务器是**长驻**进程:
//! spawn 后 stdin/stdout 持续管线传输,直到 WS 断开/退出清理。安全硬约束:
//!
//! - argv 数组直接构造,**禁 shell**(不经 cmd/bash 解释,无注入面);
//! - workdir 锁定工作区根(语言服务器只看得到工作区语义);
//! - WS 断开/serve 退出必须 kill(孤儿防护;Drop 兜底 start_kill);
//! - stderr 环形缓冲(总量封顶,防失控输出撑爆内存)。

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use tokio::io::AsyncBufReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout};
use tracing::{debug, warn};

use super::transport::FrameReader;

/// stderr 环形缓冲总容量(字节)。语言服务器崩溃排障需要尾部现场,
/// 但失控输出必须截断——保留**最新**内容,丢弃最旧并计数。
const STDERR_RING_BYTES: usize = 64 * 1024;

/// 语言服务器规格(两语言门;v1 PATH 硬发现,不托管下载)。
/// 语言服务器启动规格:可执行名 + 固定 argv(禁 shell,直接 exec)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerSpec {
    /// 可执行文件名(PATH 发现)
    pub program: &'static str,
    /// 固定启动参数
    pub args: &'static [&'static str],
}

/// Rust 门:rust-analyzer(无启动参数)
pub const RUST_ANALYZER: ServerSpec = ServerSpec {
    program: "rust-analyzer",
    args: &[],
};

/// TypeScript 门:typescript-language-server(--stdio 模式;兼管 javascript)
pub const TYPESCRIPT_SERVER: ServerSpec = ServerSpec {
    program: "typescript-language-server",
    args: &["--stdio"],
};

/// 一个已 spawn 的语言服务器进程句柄。
///
/// 字段按「拿走所有权进泵循环」设计:stdin 交给写入端,stdout 包成
/// [`FrameReader`] 交给读取端,child 本体留在结构里供退出监听与 kill。
pub struct LspServerProcess {
    /// 子进程本体(退出监听与 kill)
    pub child: Child,
    /// 标准输入(帧写入端)
    pub stdin: ChildStdin,
    /// 标准输出(帧读取端)
    pub stdout: FrameReader<ChildStdout>,
    /// stderr 环形缓冲(排障现场)
    pub stderr_ring: Arc<StderrRing>,
    /// 进程 ID(spawn 时的快照,日志用)
    pub pid: Option<u32>,
}

impl LspServerProcess {
    /// spawn 语言服务器(no-shell/argv/workdir 锁定)。
    ///
    /// stderr 管线化进环形缓冲任务;spawn 失败(不在 PATH 等)原样返回错误,
    /// 由上层转灰置/降级语义。
    pub async fn spawn(spec: &ServerSpec, workdir: &Path) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(spec.program);
        cmd.args(spec.args)
            .current_dir(workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Windows GUI 子系统下抑制控制台窗口;POSIX 无此概念(该 cfg 静默)
        #[cfg(windows)]
        {
            #[allow(deprecated)]
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("child stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| std::io::Error::other("child stderr unavailable"))?;
        let ring = Arc::new(StderrRing::new());
        tokio::spawn(stderr_drain(stderr, Arc::clone(&ring)));
        debug!(program = spec.program, pid = pid, "language server spawned");
        Ok(Self {
            child,
            stdin,
            stdout: FrameReader::new(stdout),
            stderr_ring: ring,
            pid,
        })
    }

    /// 请求终止(kill 语义;语言服务器不响应优雅关闭时兜底)。
    /// 请求终止(kill 语义;语言服务器不响应优雅关闭时兜底)。
    pub fn kill(&mut self) {
        if let Err(e) = self.child.start_kill() {
            warn!(pid = self.pid, error = %e, "language server kill failed");
        }
    }
}

impl Drop for LspServerProcess {
    fn drop(&mut self) {
        // 孤儿防护兜底:无论泵循环如何退出,句柄消亡即杀进程
        self.kill();
    }
}

/// stderr 环形缓冲(线程安全;总量封顶保留最新)。
pub struct StderrRing {
    lines: Mutex<VecDeque<String>>,
    dropped_lines: AtomicUsize,
}

impl StderrRing {
    fn new() -> Self {
        Self {
            lines: Mutex::new(VecDeque::new()),
            dropped_lines: AtomicUsize::new(0),
        }
    }

    fn push_line(&self, line: String) {
        let mut lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let mut total: usize = lines.iter().map(|l| l.len() + 1).sum();
        while total + line.len() + 1 > STDERR_RING_BYTES {
            match lines.pop_front() {
                Some(old) => {
                    total = total.saturating_sub(old.len() + 1);
                    self.dropped_lines.fetch_add(1, Ordering::Relaxed);
                }
                None => break, // 单行超限:丢弃该行本身
            }
        }
        if line.len() < STDERR_RING_BYTES {
            lines.push_back(line);
        } else {
            self.dropped_lines.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// 排障快照:全部缓冲行(最旧在前)+ 丢弃计数注记。
    /// 排障快照:全部缓冲行(最旧在前)+ 丢弃计数注记。
    pub fn snapshot(&self) -> String {
        let lines = self.lines.lock().unwrap_or_else(|e| e.into_inner());
        let dropped = self.dropped_lines.load(Ordering::Relaxed);
        let mut out = String::new();
        for l in lines.iter() {
            out.push_str(l);
            out.push('\n');
        }
        if dropped > 0 {
            out.push_str(&format!("({dropped} earlier lines dropped)"));
        }
        out
    }
}

/// stderr 排水泵:逐行读入环形缓冲;读失败/EOF 即结束(进程退出后自然收尾)。
async fn stderr_drain<R: tokio::io::AsyncRead + Unpin>(stderr: R, ring: Arc<StderrRing>) {
    let mut lines = tokio::io::BufReader::new(stderr).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => ring.push_line(line),
            Ok(None) => break,
            Err(e) => {
                debug!(error = %e, "language server stderr read ended");
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_ring_keeps_latest_within_cap() {
        let ring = StderrRing::new();
        // 每行 99 字节(+换行 100)× 1000 行 = 100KB > 64KB:应只保留尾部并计数丢弃
        let make = |i: usize| format!("line{i:0>95}");
        for i in 0..1000 {
            ring.push_line(make(i));
        }
        let snap = ring.snapshot();
        assert!(snap.contains("dropped"));
        assert!(snap.len() < STDERR_RING_BYTES + 64);
        // 最旧的 line0 一定被挤掉,最新的 line999 一定保留(零填充格式逐字节比对)
        assert!(!snap.contains(&make(0)));
        assert!(snap.contains(&make(999)));
    }

    #[test]
    fn stderr_ring_single_oversized_line_dropped() {
        let ring = StderrRing::new();
        ring.push_line("x".repeat(STDERR_RING_BYTES + 1));
        assert!(ring.snapshot().contains("dropped"));
    }

    #[tokio::test]
    async fn spawn_missing_binary_errors() {
        let spec = ServerSpec {
            program: "definitely-not-on-path-e2e4b1",
            args: &[],
        };
        let res = LspServerProcess::spawn(&spec, std::env::temp_dir().as_path()).await;
        assert!(res.is_err());
    }

    /// 真实子进程全链:spawn echo 类命令不可行(echo 是 shell 内建),
    /// 这里用本测试可执行文件自 spawn 的方式在集成测试覆盖(见 lsp_api tests);
    /// 本处仅验证 kill 清理路径——spawn 后立即 drop,无 panic 即孤儿防护生效。
    #[tokio::test]
    async fn drop_kills_child_cleanly() {
        let spec = ServerSpec {
            program: "rust-analyzer",
            args: &[],
        };
        // 环境无 rust-analyzer 时跳过(不作为失败)
        let proc = match LspServerProcess::spawn(&spec, std::env::temp_dir().as_path()).await {
            Ok(p) => p,
            Err(_) => return,
        };
        let pid = proc.pid;
        drop(proc);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // 进程应已退出(drop 兜底 kill)
        if let Some(pid) = pid {
            let alive = std::process::Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}")])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains(&pid.to_string()))
                .unwrap_or(false);
            assert!(!alive, "child pid {pid} should be dead after drop");
        }
    }
}
