// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LSP stdio 传输层:JSON-RPC Content-Length 帧编解码。
//!
//! 语言服务器经 stdio 通信,帧格式(LSP 规范 base protocol):
//!
//! ```text
//! Content-Length: <字节数>\r\n
//! [\r\n 分隔的其他头,当前仅透传解析 Content-Length]\r\n
//! \r\n
//! <恰好该字节数的 UTF-8 JSON 正文>
//! ```
//!
//! 关键口径:**长度按 UTF-8 字节数计,不是字符数**(多字节内容错算即帧错位,
//! 后续全部解析报废)。畸形帧(缺头/长度非数字/正文不足/超限)一律拒收报错,
//! 由上层决定断链——代理场景下无法「跳过坏帧」恢复同步。

#![forbid(unsafe_code)]

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncReadExt};

/// 单帧正文大小上限(防畸形帧/失控帧内存攻击)。
/// 语言服务器大仓通知(全量诊断/大补全)实测在 MB 级,64MB 留足余量;
/// 超限帧拒收,上层断链。与 WS 侧上限(4MB)独立,见 lsp_api 模块。
pub const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// 编码一帧:头部 + 正文。长度取 `body.len()`(调用方保证已是 UTF-8 字节序列)。
pub fn encode_frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 64);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    out
}

/// 从异步读取端解码一帧,返回正文字节。
///
/// 逐行读头部直到空行,解析 `Content-Length`(唯一必需头;`Content-Type` 等其余
/// 头按规范允许出现,忽略),再精确读取该字节数正文。
async fn read_frame<R>(reader: &mut R) -> io::Result<Vec<u8>>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF:头序列未闭合即流结束(服务器进程退出)
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before header complete",
            ));
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break; // 头结束
        }
        if let Some(value) = parse_header_value(line, "content-length") {
            let len: usize = value.trim().parse().map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed Content-Length value")
            })?;
            content_length = Some(len);
        }
        // 其余头(Content-Type 等)按规范忽略
    }
    let len = content_length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame size {len} exceeds limit {MAX_FRAME_BYTES}"),
        ));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await?;
    Ok(body)
}

/// 解析 `Name: value` 头(name 大小写不敏感;命中返回 value)
fn parse_header_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (n, v) = line.split_once(':')?;
    if n.trim().eq_ignore_ascii_case(name) {
        Some(v)
    } else {
        None
    }
}

/// 帧读取器:包一层 BufReader,逐帧产正文。
///
/// 独立结构体而非裸函数,便于在 select! 循环里持有所属 reader。
pub struct FrameReader<R> {
    inner: tokio::io::BufReader<R>,
}

impl<R> FrameReader<R>
where
    R: tokio::io::AsyncRead + Unpin,
{
    /// 包装任意异步读取端(内部 64KB 读缓冲)
    pub fn new(inner: R) -> Self {
        Self {
            inner: tokio::io::BufReader::with_capacity(64 * 1024, inner),
        }
    }

    /// 读取下一帧正文;EOF/畸形帧/超限均返回 Err(语义见 [`read_frame`])。
    /// 读取下一帧正文;EOF/畸形帧/超限均返回 Err(语义见 [`read_frame`])。
    pub async fn next_frame(&mut self) -> io::Result<Vec<u8>> {
        read_frame(&mut self.inner).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame(body: &str) -> Vec<u8> {
        encode_frame(body.as_bytes())
    }

    #[tokio::test]
    async fn roundtrip_ascii() {
        let mut r = FrameReader::new(Cursor::new(frame(r#"{"jsonrpc":"2.0"}"#)));
        let out = r.next_frame().await.unwrap();
        assert_eq!(out, br#"{"jsonrpc":"2.0"}"#);
    }

    #[tokio::test]
    async fn roundtrip_multibyte_utf8() {
        // 多字节正文:长度必须是字节数而非字符数(中文 3 字节/emoji 4 字节)
        let body = "{\"text\":\"中文🙂\"}";
        let byte_len = body.len(); // 9 ASCII + 3*2 + 4 + 2 = 21
        assert_eq!(byte_len, 21);
        let mut r = FrameReader::new(Cursor::new(frame(body)));
        let out = r.next_frame().await.unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), body);
        // EOF 后再读报错
        assert!(r.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn consecutive_frames() {
        let mut buf = frame("first");
        buf.extend(frame("second-帧"));
        let mut r = FrameReader::new(Cursor::new(buf));
        assert_eq!(r.next_frame().await.unwrap(), b"first");
        assert_eq!(r.next_frame().await.unwrap(), "second-帧".as_bytes());
    }

    #[tokio::test]
    async fn extra_headers_ignored() {
        let raw = b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\nContent-Length: 2\r\n\r\nok";
        let mut r = FrameReader::new(Cursor::new(&raw[..]));
        assert_eq!(r.next_frame().await.unwrap(), b"ok");
    }

    #[tokio::test]
    async fn malformed_missing_header_rejected() {
        let mut r = FrameReader::new(Cursor::new(&b"\r\nhello"[..]));
        assert!(r.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn malformed_bad_length_rejected() {
        let raw = b"Content-Length: abc\r\n\r\n{}";
        let mut r = FrameReader::new(Cursor::new(&raw[..]));
        assert!(r.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn truncated_body_rejected() {
        let raw = b"Content-Length: 100\r\n\r\nshort";
        let mut r = FrameReader::new(Cursor::new(&raw[..]));
        assert!(r.next_frame().await.is_err());
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let raw = format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1);
        let mut r = FrameReader::new(Cursor::new(raw.into_bytes()));
        let err = r.next_frame().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn header_parse_case_insensitive() {
        assert_eq!(
            parse_header_value("CONTENT-LENGTH: 5", "content-length"),
            Some(" 5")
        );
        assert_eq!(
            parse_header_value("content-length:5", "Content-Length"),
            Some("5")
        );
        assert_eq!(
            parse_header_value("Content-Type: text", "content-length"),
            None
        );
    }

    #[test]
    fn encode_shape() {
        let out = String::from_utf8(encode_frame(b"{}")).unwrap();
        assert_eq!(out, "Content-Length: 2\r\n\r\n{}");
    }
}
