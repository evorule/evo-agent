// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
//! serve 日志环形缓冲(G1 控制台日志数据源):
//! tracing 输出经 Tee writer 双写——stdout 原样 + 内存环形缓冲(最近 500 条),
//! `GET /admin/logs?n=200` 读缓冲尾部,前端控制台日志 tab 2s 轮询呈现。
//! 纯内存零落盘,进程重启即清(控制台日志是排障视图,不充当审计链)。
use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Mutex, OnceLock};

/// 缓冲容量(条目数);每条=一次 tracing 事件格式化输出(通常一行,消息内含换行则多行)
const RING_CAP: usize = 500;

fn ring() -> &'static Mutex<VecDeque<String>> {
    static RING: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();
    RING.get_or_init(|| Mutex::new(VecDeque::with_capacity(RING_CAP)))
}

/// fmt 层 MakeWriter:每次事件生成一个 TeeWriter,Drop 时整块入环+转发 stdout
#[derive(Clone, Copy, Default)]
pub struct LogTee;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogTee {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            buf: Vec::new(),
            to_stderr: false,
        }
    }
}

/// stderr 变体:结构化事件面(CLI run --json 的 stdout NDJSON 契约)专用——
/// 日志转发 stderr,stdout 只承载事件帧;环形缓冲行为与 stdout 变体一致
#[derive(Clone, Copy, Default)]
pub struct LogTeeStderr;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogTeeStderr {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            buf: Vec::new(),
            to_stderr: true,
        }
    }
}

/// 单事件收集器:fmt 层把格式化好的一条日志写入 buf,Drop 时双写
pub struct TeeWriter {
    buf: Vec<u8>,
    /// 转发目标:false=stdout(默认,进程既有行为),true=stderr(结构化事件面)
    to_stderr: bool,
}

impl Write for TeeWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for TeeWriter {
    fn drop(&mut self) {
        // 转发(默认 stdout 原样;stderr 变体服务 stdout 纯净的 JSON 事件面)
        if self.to_stderr {
            let _ = std::io::stderr().write_all(&self.buf);
            let _ = std::io::stderr().flush();
        } else {
            let _ = std::io::stdout().write_all(&self.buf);
            let _ = std::io::stdout().flush();
        }
        // 环形缓冲(截超长单条防内存尖刺;锁毒化时丢弃本条,日志面不致命)
        let mut text = String::from_utf8_lossy(&self.buf).into_owned();
        text.truncate(text.trim_end_matches('\n').len()); // 去 fmt 层尾换行,入环归一
        let mut text = text.trim_end().to_string();
        const MAX_ENTRY: usize = 8 * 1024;
        if text.len() > MAX_ENTRY {
            let mut cut = MAX_ENTRY;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str(" …(截断)");
        }
        if !text.is_empty() {
            if let Ok(mut ring) = ring().lock() {
                if ring.len() >= RING_CAP {
                    ring.pop_front();
                }
                ring.push_back(text);
            }
        }
    }
}

/// 端点读缓冲尾部 n 条(按产出顺序)
pub fn tail_lines(n: usize) -> Vec<String> {
    match ring().lock() {
        Ok(ring) => {
            let skip = ring.len().saturating_sub(n);
            ring.iter().skip(skip).cloned().collect()
        }
        Err(_) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    /// 全局 ring 为进程级单例,测试共享(并行会互踩)——两场景合并串行断言
    #[test]
    fn ring_keeps_last_entries_and_truncates_oversized() {
        // 场景 1:容量轮转(600 条只留最近 500,tail(200) 为最后 200 条)
        // fmt 层真实语义:每个事件独立 make_writer→write→drop,逐条模拟
        for i in 0..600 {
            let mut w = Box::new(LogTee.make_writer()) as Box<dyn Write>;
            write!(w, "line {i}").unwrap();
            drop(w);
        }
        let tail = tail_lines(200);
        assert_eq!(tail.len(), 200);
        assert_eq!(tail.last().unwrap(), "line 599");
        assert!(!tail.iter().any(|l| l == "line 399"));

        // 场景 2:超长单条截断(不整条吞掉,尾部带标记)
        let mut w = Box::new(LogTee.make_writer()) as Box<dyn Write>;
        let long = "x".repeat(20 * 1024) + "尾巴标记";
        write!(w, "{long}").unwrap();
        drop(w);
        let tail = tail_lines(1);
        assert_eq!(tail.len(), 1);
        assert!(tail[0].contains("…(截断)"));
        assert!(!tail[0].contains("尾巴标记"));
    }
}
