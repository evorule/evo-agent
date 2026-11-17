// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
#![forbid(unsafe_code)]
//! 流式字节块 → 文本的安全累积器。
//!
//! 逐网络块独立 `from_utf8_lossy` 解码会把「被 TCP/TLS 分块切开的多字节
//! UTF-8 字符」替换成 U+FFFD——随机坏字（长会话高频 CJK 写入实测，存量
//! 缺陷账 o10 ①型）的传输层根因。本累积器持有跨块的不完整多字节尾部，
//! 只解码最长有效 UTF-8 前缀，尾部留待续块拼接；流中部出现真损坏
//! （非法字节而非尾部截断）时 fail-visible 上抛，不 lossy 不猜。
//!
//! ## 用法
//!
//! ```ignore
//! let mut dec = StreamTextDecoder::new();
//! for chunk in byte_stream {
//!     let text = dec.feed(&chunk)?;
//!     text_buffer.push_str(&text);
//! }
//! ```
//!
//! ## EOF 语义
//!
//! 流结束时若 `tail` 仍非空 = 流在多字节字符中部被截断。调用方的既有
//! 完整性检查（如 SSE [DONE] 终止符缺席即报错）已覆盖该场景——截断流
//! 本就 fail-visible，此处不重复报错也不 lossy 补码。

/// 流式 UTF-8 安全解码累积器
#[derive(Debug, Default)]
pub struct StreamTextDecoder {
    /// 跨块持有的未解码字节(通常=不完整多字节序列尾部,或半条 SSE 帧)
    tail: Vec<u8>,
}

impl StreamTextDecoder {
    /// 创建空累积器(无预持字节)
    pub fn new() -> Self {
        Self { tail: Vec::new() }
    }

    /// 喂入一个原始字节块,返回本次可安全解码的文本增量。
    ///
    /// - 尾部不完整多字节序列 → 留在内部缓冲,续块到达后拼接解码;
    /// - 流中部真损坏(非法起始字节) → `Err`(fail-visible,携带字节位);
    /// - 返回值恒为有效 UTF-8(空串也是合法增量)。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<String, String> {
        self.tail.extend_from_slice(chunk);
        let up_to = match std::str::from_utf8(&self.tail) {
            Ok(_) => self.tail.len(),
            Err(e) => match e.error_len() {
                // 尾部不完整序列(被块边界切开的多字节字符):留待续块
                None => e.valid_up_to(),
                // 真损坏:非尾部位置的非法字节——fail-visible
                Some(bad) => {
                    return Err(format!(
                        "stream contains invalid UTF-8 at byte {} ({} invalid byte(s)): \
                         transport corruption suspected",
                        e.valid_up_to(),
                        bad
                    ));
                }
            },
        };
        let text = std::str::from_utf8(&self.tail[..up_to])
            .expect("up_to 由 from_utf8 校验,必然有效")
            .to_string();
        self.tail.drain(..up_to);
        Ok(text)
    }

    /// 流结束:内部缓冲应已耗尽(所有完整帧都已吐出)。残留非空=流在
    /// 多字节字符中部被截断,返回残留字节长度供调用方 fail-visible。
    pub fn pending_tail_len(&self) -> usize {
        self.tail.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // o10 ①型复现:CJK「好」= E5 A5 BD 三字节,切块切在任意字节位
    #[test]
    fn cjk_split_across_chunks_reassembles_cleanly() {
        let full = "好".as_bytes();
        // 三种切块位:E5|A5BD / E5A5|BD / E5A5BD| — 全部应无损
        for split in 1..3 {
            let mut dec = StreamTextDecoder::new();
            let t1 = dec.feed(&full[..split]).unwrap();
            let t2 = dec.feed(&full[split..]).unwrap();
            assert_eq!(format!("{t1}{t2}"), "好", "split at {split}");
        }
    }

    #[test]
    fn cjk_boundary_inside_json_sse_frame() {
        // 实战形态:SSE 帧内 JSON 串含 CJK,切块切在 JSON 值中部的 CJK 上
        let payload = "{\"delta\":\"交付文档状态更新并收口：已按核实\"}".to_string();
        let bytes = payload.as_bytes();
        // 在每个可能位置切块,拼接结果必须逐字节等于原payload
        for split in 1..bytes.len() {
            let mut dec = StreamTextDecoder::new();
            let a = dec.feed(&bytes[..split]).unwrap();
            let b = dec.feed(&bytes[split..]).unwrap();
            assert_eq!(format!("{a}{b}"), payload, "split at {split}");
        }
    }

    #[test]
    fn cjk_line_by_line_stream_no_fffd() {
        // o10 实录形态:多行 CJK 文本按网络块随机切——13+ 例坏字的复现面
        let text =
            "交付文档状态更新并收口：o10 写通道坏字\n已按核实结论更新文档状态并收口：commit 已落\n";
        let bytes = text.as_bytes();
        // 模拟 7 字节块流(必然切开多字节字符)
        let mut dec = StreamTextDecoder::new();
        let mut acc = String::new();
        for chunk in bytes.chunks(7) {
            acc.push_str(&dec.feed(chunk).unwrap());
        }
        assert_eq!(acc, text);
        assert!(!acc.contains('\u{FFFD}'), "零 U+FFFD");
    }

    #[test]
    fn ascii_stream_unchanged() {
        // ASCII 不受影响(回归护栏:修复不得改变纯 ASCII 流行为)
        let mut dec = StreamTextDecoder::new();
        let a = dec.feed(b"cargo test --release\n").unwrap();
        assert_eq!(a, "cargo test --release\n");
    }

    #[test]
    fn genuine_midstream_corruption_is_fail_visible() {
        // 真损坏:流中部非法起始字节(E0 后跟非法续字节)→ Err 不 lossy
        let mut dec = StreamTextDecoder::new();
        dec.feed(b"ok ").unwrap();
        let bad = [b'o', b'k', 0xE0, 0x41]; // E0 需两个续字节,0x41 非法
        let err = dec.feed(&bad).unwrap_err();
        assert!(err.contains("invalid UTF-8"), "got: {err}");
        assert!(err.contains("transport corruption"), "got: {err}");
    }

    #[test]
    fn multibyte_char_split_across_three_chunks() {
        // 三块接力:「状」三字节按 1+1+1 切——每块后尾部在册,拼齐即解出
        let mut dec = StreamTextDecoder::new();
        let full = "状".as_bytes(); // E7 8A B6
        let t1 = dec.feed(&full[..1]).unwrap();
        assert_eq!(t1, "", "单首字节无可输出");
        let t2 = dec.feed(&full[1..2]).unwrap();
        assert_eq!(t2, "", "两字节仍不完整");
        let t3 = dec.feed(&full[2..]).unwrap();
        assert_eq!(t3, "状", "三字节拼齐即解出");
    }

    #[test]
    fn pending_tail_len_tracks_incomplete_tail() {
        let mut dec = StreamTextDecoder::new();
        let full = "好".as_bytes(); // E5 A5 BD
        dec.feed(&full[..1]).unwrap();
        assert_eq!(dec.pending_tail_len(), 1, "单字节尾部在册");
        dec.feed(&full[1..]).unwrap();
        assert_eq!(dec.pending_tail_len(), 0, "完整字符解出后尾部清零");
    }
}
