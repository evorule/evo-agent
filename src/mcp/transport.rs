// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! G12:JSON-RPC 2.0 传输层 —— MCP 协议的底层管道
//!
//! ## 协议概要
//!
//! MCP 基于 [JSON-RPC 2.0](https://www.jsonrpc.org/specification),核心交互:
//!
//! ```text
//! Client → Server:  {"jsonrpc":"2.0","method":"initialize","params":{...},"id":1}
//! Server → Client:  {"jsonrpc":"2.0","result":{...},"id":1}
//! Client → Server:  {"jsonrpc":"2.0","method":"tools/list","id":2}
//! Server → Client:  {"jsonrpc":"2.0","result":{"tools":[...]},"id":2}
//! Client → Server:  {"jsonrpc":"2.0","method":"tools/call","params":{"name":"...","arguments":{...}},"id":3}
//! Server → Client:  {"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"..."}]},"id":3}
//! ```
//!
//! ## stdio 传输
//!
//! [`StdioTransport`] spawn 一个子进程,通过子进程的 stdin/stdout 收发 JSON-RPC 消息
//! (每行一条 JSON,以 `\n` 结尾)。这是 Claude Desktop 等最常用的传输方式。
//!
//! ### 并发模型
//!
//! - **写入**:多个 `request()` 并发调用时,通过 `tokio::sync::Mutex<ChildStdin>`
//!   串行化 stdin 写入(每条消息原子写入,不会被交错)
//! - **读取**:一个独立的 reader task 持有 stdout,逐行读取,按 JSON-RPC `id`
//!   路由到对应的 `oneshot` channel
//! - **id 分配**:`AtomicU64` 自增,保证并发请求 id 不冲突
//!
//! ### 错误处理
//!
//! - 子进程退出(stdout EOF)→ reader task 结束 → 所有 pending request 的
//!   oneshot channel 被关闭 → `request()` 返回 `"MCP server closed connection"` 错误
//! - JSON-RPC error 响应(`{"error":{"message":...}}`)→ `request()` 返回该 message

use std::collections::HashMap;
use std::process::Stdio as StdIo;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{oneshot, Mutex};
use tracing::{debug, warn};

/// JSON-RPC 2.0 传输层 trait
///
/// P1 只实现 [`StdioTransport`](StdioTransport);P2 可加 SseTransport。
#[async_trait]
#[allow(clippy::double_must_use)] // async_trait 宏生成的 must_use 与 Boxed Future 类型语义重复,源上无法消
pub trait McpTransport: Send + Sync {
    /// 发送一条 JSON-RPC 请求,等待响应
    ///
    /// `method` 如 `"initialize"` / `"tools/list"` / `"tools/call"`。
    /// `params` 作为 JSON-RPC `params` 字段。
    async fn request(&self, method: &str, params: Value) -> Result<Value, String>;

    /// 发送通知(不等待响应,无 `id`)
    ///
    /// 如 MCP 握手的 `notifications/initialized`。
    async fn notify(&self, method: &str, params: Value) -> Result<(), String>;

    /// 关闭连接(终止子进程)
    async fn close(&self) -> Result<(), String>;
}

/// pending 请求的回调类型:成功返回 Value,失败返回错误描述
type PendingResult = Result<Value, String>;

/// stdio 传输:子进程 stdin/stdout
///
/// 生命周期:一个 `StdioTransport` 对应一个 MCP server 子进程。
/// drop 时不会自动杀子进程(需显式调 [`close`](McpTransport::close)),
/// 但子进程通常在 server 主动退出或 evo-agent 退出时终止。
pub struct StdioTransport {
    /// 子进程(close 时 kill)
    child: Mutex<Child>,
    /// stdin(串行化写入,避免并发 request 交错)
    stdin: Mutex<ChildStdin>,
    /// 自增的 JSON-RPC id
    next_id: AtomicU64,
    /// 等待响应的 channel:id → oneshot sender
    /// 用 std::sync::Mutex(锁内无 await,持有时间极短)
    pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>>,
}

impl StdioTransport {
    /// spawn 一个 MCP server 子进程并启动 reader loop
    ///
    /// `command` 如 `"npx"`,`args` 如 `["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]`。
    /// 子进程的 stderr 会被丢弃(不 pipe,继承父进程;如需捕获可后续扩展)。
    pub async fn spawn(command: &str, args: &[&str]) -> Result<Self, String> {
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(StdIo::piped())
            .stdout(StdIo::piped())
            .stderr(StdIo::null())
            .spawn()
            .map_err(|e| format!("spawn MCP server '{}' failed: {}", command, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP child has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP child has no stdout".to_string())?;

        let pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));

        // 启动 stdout reader loop(独立 tokio task)
        let pending_clone = pending.clone();
        tokio::spawn(reader_loop(stdout, pending_clone));

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending,
        })
    }

    /// spawn 时附带环境变量(用于 MCP server 需要 token 等场景)
    ///
    /// `env` 会合并到子进程环境(不覆盖父进程未列出的变量)。
    pub async fn spawn_with_env(
        command: &str,
        args: &[&str],
        env: &HashMap<String, String>,
    ) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(StdIo::piped())
            .stdout(StdIo::piped())
            .stderr(StdIo::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("spawn MCP server '{}' failed: {}", command, e))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP child has no stdin".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP child has no stdout".to_string())?;

        let pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        let pending_clone = pending.clone();
        tokio::spawn(reader_loop(stdout, pending_clone));

        Ok(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending,
        })
    }
}

/// reader loop:逐行读 stdout,解析 JSON-RPC response,按 id 路由到 pending channel
///
/// - 成功响应(`{"result":...}`)→ 发送 `Ok(result)`
/// - 错误响应(`{"error":{"message":...}}`)→ 发送 `Err(message)`
/// - 通知(无 `id`)→ 仅 debug 日志(P1 不处理 server→client 通知)
/// - EOF / 解析失败 → 退出 loop;剩余 pending 由 oneshot 关闭自动报错
async fn reader_loop(
    stdout: ChildStdout,
    pending: Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>>,
) {
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                // EOF:server 关闭了 stdout
                debug!("MCP server stdout EOF, reader loop exiting");
                drain_pending(&pending, "MCP server closed connection");
                return;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parsed: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(raw = %trimmed, error = %e, "MCP stdout: failed to parse line as JSON, skipping");
                        continue;
                    }
                };
                // 通知(无 id)→ 仅记录
                let id = match parsed.get("id").and_then(|v| v.as_u64()) {
                    Some(id) => id,
                    None => {
                        let method = parsed.get("method").and_then(|v| v.as_str()).unwrap_or("?");
                        debug!(method = %method, "MCP server notification (no id), ignoring");
                        continue;
                    }
                };
                // 取出 pending sender(若有)
                let sender = {
                    let mut map = match pending.lock() {
                        Ok(m) => m,
                        Err(_) => {
                            warn!("MCP pending map poisoned, reader loop exiting");
                            return;
                        }
                    };
                    map.remove(&id)
                };
                let Some(sender) = sender else {
                    warn!(
                        id,
                        "MCP response has id but no pending request (maybe timed out?), dropping"
                    );
                    continue;
                };
                // 路由 result / error
                if let Some(err) = parsed.get("error") {
                    let msg = err
                        .get("message")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown MCP error")
                        .to_string();
                    let _ = sender.send(Err(msg));
                } else {
                    let result = parsed.get("result").cloned().unwrap_or(Value::Null);
                    let _ = sender.send(Ok(result));
                }
            }
            Err(e) => {
                warn!(error = %e, "MCP stdout read error, reader loop exiting");
                drain_pending(&pending, &format!("MCP stdout read error: {}", e));
                return;
            }
        }
    }
}

/// drain pending map,给所有等待者发送错误(用于 EOF / 退出场景)
fn drain_pending(
    pending: &Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<PendingResult>>>>,
    reason: &str,
) {
    let mut map = match pending.lock() {
        Ok(m) => m,
        Err(_) => return,
    };
    for (_id, sender) in map.drain() {
        let _ = sender.send(Err(reason.to_string()));
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .map_err(|_| "MCP pending map poisoned".to_string())?;
            map.insert(id, tx);
        }

        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });

        // 写入 stdin(串行化,每条消息一行)
        let line =
            serde_json::to_string(&req).map_err(|e| format!("serialize MCP request: {}", e))?;
        {
            let mut stdin = self.stdin.lock().await;
            stdin
                .write_all(format!("{}\n", line).as_bytes())
                .await
                .map_err(|e| format!("write to MCP stdin: {}", e))?;
            stdin
                .flush()
                .await
                .map_err(|e| format!("flush MCP stdin: {}", e))?;
        }

        debug!(method = %method, id, "MCP request sent");

        // 等待响应(reader loop 按 id 路由)
        rx.await
            .map_err(|_| "MCP response channel closed".to_string())?
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line =
            serde_json::to_string(&req).map_err(|e| format!("serialize MCP notify: {}", e))?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("{}\n", line).as_bytes())
            .await
            .map_err(|e| format!("write to MCP stdin: {}", e))?;
        stdin
            .flush()
            .await
            .map_err(|e| format!("flush MCP stdin: {}", e))?;
        debug!(method = %method, "MCP notify sent");
        Ok(())
    }

    async fn close(&self) -> Result<(), String> {
        let mut child = self.child.lock().await;
        // 先尝试 kill,再 wait(避免僵尸进程)
        if let Err(e) = child.kill().await {
            warn!(error = %e, "MCP child kill failed (maybe already exited)");
        }
        let _ = child.wait().await;
        // 通知所有 pending 请求
        drain_pending(&self.pending, "MCP transport closed");
        Ok(())
    }
}

/// 展开 `${ENV_VAR}` 引用(B23)
///
/// streamable_http 模式的 headers 值支持环境变量引用(如
/// `"Authorization": "Bearer ${MCP_TOKEN}"`),避免明文密钥落配置文件。
/// 变量缺失 → fail-closed 报错(不静默替换空值,防凭据语义被静默削弱)。
fn expand_env_refs(value: &str) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!("unclosed ${{ in header value: {}", value));
        };
        let var = &after[..end];
        if var.is_empty() {
            return Err(format!("empty ${{}} reference in header value: {}", value));
        }
        match std::env::var(var) {
            Ok(v) => out.push_str(&v),
            Err(_) => {
                return Err(format!(
                    "environment variable '{}' referenced in MCP header not set",
                    var
                ))
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Streamable HTTP 传输(B23):远程 MCP server,按 MCP 规范 Streamable HTTP
///
/// 与已废弃的 HTTP+SSE 传输(2024-11-05,SEP-2596 deprecated)不同:
/// 单一 endpoint,JSON-RPC 请求经 `POST`,响应可为单 JSON 或 SSE 流;
/// 会话经 `Mcp-Session-Id` 响应头建立、`DELETE` 终止。
pub struct StreamableHttpTransport {
    /// MCP server endpoint URL
    url: String,
    /// 已展开 env 的附加请求头(如 Authorization)
    headers: Vec<(String, String)>,
    client: reqwest::Client,
    /// 自增的 JSON-RPC id
    next_id: AtomicU64,
    /// `initialize` 响应捕获的会话 id(后续请求带上;无会话则为 None)
    session_id: std::sync::Mutex<Option<String>>,
}

impl StreamableHttpTransport {
    /// 构造传输(展开 headers 中的 `${ENV_VAR}` 引用,fail-closed)
    pub fn new(url: &str, headers: &HashMap<String, String>) -> Result<Self, String> {
        if url.trim().is_empty() {
            return Err("MCP streamable_http url must not be empty".to_string());
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!(
                "MCP streamable_http url must start with http:// or https://, got: {}",
                url
            ));
        }
        let mut resolved = Vec::with_capacity(headers.len());
        for (k, v) in headers {
            resolved.push((k.clone(), expand_env_refs(v)?));
        }
        Ok(Self {
            url: url.to_string(),
            headers: resolved,
            client: reqwest::Client::new(),
            next_id: AtomicU64::new(1),
            session_id: std::sync::Mutex::new(None),
        })
    }

    /// 组装基础 POST 请求(Accept 双类型 + 附加头 + 会话头)
    fn post(&self, body: &Value) -> Result<reqwest::RequestBuilder, String> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", crate::mcp::client::PROTOCOL_VERSION)
            .json(body);
        if let Some(sid) = self
            .session_id
            .lock()
            .map_err(|_| "MCP session lock poisoned".to_string())?
            .clone()
        {
            req = req.header("Mcp-Session-Id", sid);
        }
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        Ok(req)
    }

    /// 捕获 `Mcp-Session-Id` 响应头(initialize 时 server 下发)
    fn capture_session(&self, resp: &reqwest::Response) {
        if let Some(sid) = resp.headers().get("mcp-session-id") {
            if let Ok(s) = sid.to_str() {
                if let Ok(mut slot) = self.session_id.lock() {
                    *slot = Some(s.to_string());
                }
            }
        }
    }

    /// 解析 SSE 流响应:按空行分事件,`data:` 行为 JSON-RPC 响应,匹配 id 即返回
    ///
    /// 单 POST 的响应流内可能夹杂其他事件(如 log/notification),仅取含
    /// 匹配 `id` 的响应;流结束仍未匹配 → 报错。
    async fn request_via_sse(&self, resp: reqwest::Response, id: u64) -> Result<Value, String> {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            match StreamExt::next(&mut stream).await {
                Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                Some(Err(e)) => return Err(format!("MCP streamable_http stream read: {}", e)),
                None => {
                    return Err(format!(
                        "MCP streamable_http SSE stream ended without response for id {}",
                        id
                    ))
                }
            }
            // 按空行(\n\n)切分完整事件;余部留在 buf
            while let Some(pos) = find_subslice(&buf, b"\n\n") {
                let event: Vec<u8> = buf.drain(..pos + 2).collect();
                if let Some(reply) = parse_sse_event(&event, id)? {
                    return Ok(reply);
                }
            }
        }
    }
}

/// 在 buffer 中定位子序列位置(用于按 `\n\n` 切分 SSE 事件)
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 解析单个 SSE 事件块:取全部 `data:` 行拼接,JSON 解析后匹配 id
///
/// 返回 `Ok(None)` 表示本事件不含匹配响应(通知/日志事件,跳过)。
fn parse_sse_event(event: &[u8], id: u64) -> Result<Option<Value>, String> {
    let text = String::from_utf8_lossy(event);
    let mut data_lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
        // `event:` 行(如 message)不参与路由——按 data 内容与 id 匹配即可
    }
    if data_lines.is_empty() {
        return Ok(None);
    }
    let payload = data_lines.join("\n");
    let parsed: Value = serde_json::from_str(&payload)
        .map_err(|e| format!("MCP streamable_http SSE data parse: {}", e))?;
    // 无 id = server 通知,跳过
    let Some(reply_id) = parsed.get("id").and_then(|v| v.as_u64()) else {
        return Ok(None);
    };
    if reply_id != id {
        return Ok(None);
    }
    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown MCP error")
            .to_string();
        return Err(msg);
    }
    Ok(Some(parsed.get("result").cloned().unwrap_or(Value::Null)))
}

#[async_trait]
impl McpTransport for StreamableHttpTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
            "id": id,
        });
        let resp = self
            .post(&body)?
            .send()
            .await
            .map_err(|e| format!("MCP streamable_http POST '{}' failed: {}", method, e))?;
        self.capture_session(&resp);
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "MCP streamable_http '{}' returned HTTP {}: {}",
                method,
                status.as_u16(),
                text
            ));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if ct.contains("text/event-stream") {
            return self.request_via_sse(resp, id).await;
        }
        // 单 JSON 响应
        let parsed: Value = resp
            .json()
            .await
            .map_err(|e| format!("MCP streamable_http '{}' body parse: {}", method, e))?;
        if let Some(err) = parsed.get("error") {
            let msg = err
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown MCP error")
                .to_string();
            return Err(msg);
        }
        if parsed.get("id").and_then(|v| v.as_u64()) != Some(id) {
            return Err(format!(
                "MCP streamable_http '{}' response id mismatch",
                method
            ));
        }
        Ok(parsed.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let resp = self
            .post(&body)?
            .send()
            .await
            .map_err(|e| format!("MCP streamable_http notify '{}' failed: {}", method, e))?;
        let status = resp.status();
        // 通知无响应体;2xx(通常 202)即成功
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(format!(
                "MCP streamable_http notify '{}' returned HTTP {}: {}",
                method,
                status.as_u16(),
                text
            ));
        }
        debug!(method = %method, "MCP streamable_http notify sent");
        Ok(())
    }

    async fn close(&self) -> Result<(), String> {
        // 会话终止:DELETE endpoint + Mcp-Session-Id(规范语义);无会话则无需
        let sid = self
            .session_id
            .lock()
            .map_err(|_| "MCP session lock poisoned".to_string())?
            .clone();
        let Some(sid) = sid else {
            return Ok(());
        };
        let mut req = self.client.delete(&self.url);
        req = req.header("Mcp-Session-Id", sid);
        for (k, v) in &self.headers {
            req = req.header(k.as_str(), v.as_str());
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => Ok(()),
            Ok(resp) => Err(format!(
                "MCP streamable_http session DELETE returned HTTP {}",
                resp.status().as_u16()
            )),
            Err(e) => Err(format!("MCP streamable_http session DELETE failed: {}", e)),
        }
    }
}

#[cfg(test)]
#[allow(missing_docs)]
pub mod tests {
    use super::*;

    /// Mock 传输:按 method 返回预设的固定响应(用于 client/adapter 单元测试)
    pub struct MockTransport {
        responses: tokio::sync::Mutex<HashMap<String, Value>>,
        notifies: tokio::sync::Mutex<Vec<(String, Value)>>,
        closed: std::sync::atomic::AtomicBool,
    }

    impl Default for MockTransport {
        fn default() -> Self {
            Self::new()
        }
    }

    impl MockTransport {
        pub fn new() -> Self {
            Self {
                responses: tokio::sync::Mutex::new(HashMap::new()),
                notifies: tokio::sync::Mutex::new(Vec::new()),
                closed: std::sync::atomic::AtomicBool::new(false),
            }
        }

        /// 注册一个 method 的响应(result 字段)
        pub async fn set_response(&self, method: &str, result: Value) {
            self.responses
                .lock()
                .await
                .insert(method.to_string(), result);
        }

        pub async fn notifies_received(&self) -> Vec<(String, Value)> {
            self.notifies.lock().await.clone()
        }

        pub fn is_closed(&self) -> bool {
            self.closed.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn request(&self, method: &str, _params: Value) -> Result<Value, String> {
            let map = self.responses.lock().await;
            map.get(method)
                .cloned()
                .ok_or_else(|| format!("MockTransport: no response registered for '{}'", method))
        }

        async fn notify(&self, method: &str, params: Value) -> Result<(), String> {
            self.notifies
                .lock()
                .await
                .push((method.to_string(), params));
            Ok(())
        }

        async fn close(&self) -> Result<(), String> {
            self.closed.store(true, Ordering::Relaxed);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_mock_transport_request_returns_registered() {
        let t = MockTransport::new();
        t.set_response("tools/list", serde_json::json!({"tools": []}))
            .await;
        let result = t
            .request("tools/list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["tools"].as_array().map(|a| a.len()), Some(0));
    }

    #[tokio::test]
    async fn test_mock_transport_request_unregistered_method() {
        let t = MockTransport::new();
        let result = t.request("unknown", serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no response registered"));
    }

    #[tokio::test]
    async fn test_mock_transport_notify_records() {
        let t = MockTransport::new();
        t.notify("notifications/initialized", serde_json::json!({}))
            .await
            .unwrap();
        let notifies = t.notifies_received().await;
        assert_eq!(notifies.len(), 1);
        assert_eq!(notifies[0].0, "notifications/initialized");
    }

    #[tokio::test]
    async fn test_mock_transport_close_sets_flag() {
        let t = MockTransport::new();
        assert!(!t.is_closed());
        t.close().await.unwrap();
        assert!(t.is_closed());
    }

    #[tokio::test]
    async fn test_stdio_transport_spawn_nonexistent_command_fails() {
        // 用一个确定不存在的命令,验证 spawn 返回友好错误
        // 不用 unwrap_err()(需要 StdioTransport: Debug),改用 .err().unwrap()
        let result =
            StdioTransport::spawn("this-command-definitely-does-not-exist-xyz123", &[]).await;
        assert!(result.is_err());
        let err = result.err().unwrap();
        assert!(err.contains("spawn MCP server") || err.contains("failed"));
    }

    /// 真实子进程 round-trip 测试(需要 python)
    ///
    /// `#[ignore]` 因为依赖外部 python 环境;手动运行:
    /// `cargo test --lib -- --ignored test_stdio_real_subprocess_round_trip`
    ///
    /// 验证 StdioTransport 的完整数据流:
    /// spawn → initialize 握手 → tools/list → tools/call → close
    #[tokio::test]
    #[ignore]
    async fn test_stdio_real_subprocess_round_trip() {
        // 嵌入一个最小 MCP server 脚本到临时文件(自包含,不依赖外部文件布局)
        let script_content = r#"import sys, json
def send(m):
    sys.stdout.write(json.dumps(m) + "\n"); sys.stdout.flush()
while True:
    line = sys.stdin.readline()
    if not line: break
    line = line.strip()
    if not line: continue
    try: req = json.loads(line)
    except: continue
    method = req.get("method"); rid = req.get("id")
    if rid is None: continue  # notification
    if method == "initialize":
        send({"jsonrpc":"2.0","id":rid,"result":{"protocolVersion":"2024-11-05","serverInfo":{"name":"mock-mcp","version":"0.1.0"},"capabilities":{}}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":rid,"result":{"tools":[{"name":"echo","description":"echo args","inputSchema":{"type":"object"}}]}})
    elif method == "tools/call":
        p = req.get("params",{}); nm = p.get("name",""); args = p.get("arguments",{})
        if nm == "echo":
            send({"jsonrpc":"2.0","id":rid,"result":{"content":[{"type":"text","text":json.dumps(args)}]}})
        else:
            send({"jsonrpc":"2.0","id":rid,"result":{"isError":True,"content":[{"type":"text","text":"unknown"}]}})
    else:
        send({"jsonrpc":"2.0","id":rid,"error":{"code":-32601,"message":"not found"}})
"#;
        let tmp = tempfile::tempdir().expect("tempdir");
        let script_path = tmp.path().join("mock_mcp.py");
        std::fs::write(&script_path, script_content).expect("write script");
        let script_str = script_path.to_str().unwrap();

        // 尝试 python / python3
        let python = ["python", "python3"]
            .iter()
            .find(|cmd| {
                std::process::Command::new(cmd)
                    .arg("--version")
                    .output()
                    .is_ok()
            })
            .copied()
            .expect("python not found on PATH");

        let transport = StdioTransport::spawn(python, &[script_str]).await.unwrap();

        // 1. initialize 握手
        let init_result = transport
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "evo-agent-test", "version": "0.1.0"}
                }),
            )
            .await
            .expect("initialize should succeed");
        assert_eq!(init_result["serverInfo"]["name"].as_str(), Some("mock-mcp"));

        // 2. 发送 initialized 通知(无响应)
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
            .expect("notify should succeed");

        // 3. tools/list
        let tools_result = transport
            .request("tools/list", serde_json::json!({}))
            .await
            .expect("tools/list should succeed");
        let tools = tools_result["tools"]
            .as_array()
            .expect("tools should be array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"].as_str(), Some("echo"));

        // 4. tools/call —— echo 工具回显参数
        let call_result = transport
            .request(
                "tools/call",
                serde_json::json!({
                    "name": "echo",
                    "arguments": {"message": "hello from MCP"}
                }),
            )
            .await
            .expect("tools/call should succeed");
        let text = call_result["content"][0]["text"]
            .as_str()
            .expect("should have text content");
        assert!(text.contains("hello from MCP"));

        // 5. close
        transport.close().await.expect("close should succeed");
    }

    /// 起 Streamable HTTP mock server(axum,绑定随机端口)
    ///
    /// 行为:initialize → 单 JSON + 下发 mcp-session-id;tools/list → SSE 流
    /// 形态;tools/call → 单 JSON 回显;DELETE → 200(会话终止)。
    async fn spawn_http_mock() -> String {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::response::IntoResponse;
        use axum::routing::post;
        use std::sync::Arc;

        #[derive(Clone, Default)]
        struct H {
            session: Arc<std::sync::atomic::AtomicBool>,
        }

        async fn handler(
            State(_h): State<H>,
            headers: HeaderMap,
            body: String,
        ) -> axum::response::Response {
            let req: Value = serde_json::from_str(&body).unwrap();
            let method = req["method"].as_str().unwrap_or("").to_string();
            let id = req["id"].clone();
            let reply =
                |result: Value| serde_json::json!({"jsonrpc":"2.0","id":id,"result":result});
            match method.as_str() {
                "initialize" => {
                    let mut resp = axum::Json(reply(serde_json::json!({
                        "protocolVersion":"2024-11-05",
                        "serverInfo":{"name":"mock-http","version":"0.1.0"},
                        "capabilities":{}
                    })))
                    .into_response();
                    resp.headers_mut().insert(
                        "mcp-session-id",
                        axum::http::HeaderValue::from_static("sess-123"),
                    );
                    resp
                }
                "tools/list" => {
                    // SSE 流形态:两个事件,第一个为无关通知,第二个为匹配响应
                    let payload = format!(
                        "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"log/notice\",\"params\":{{}}}}\n\n\
                         event: message\ndata: {}\n\n",
                        serde_json::to_string(&reply(serde_json::json!({"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}))).unwrap()
                    );
                    let _ = headers;
                    axum::response::Response::builder()
                        .status(200)
                        .header("content-type", "text/event-stream")
                        .body(axum::body::Body::from(payload))
                        .unwrap()
                }
                "tools/call" => {
                    let text = req["params"]["arguments"]["message"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    axum::Json(reply(serde_json::json!({
                        "content":[{"type":"text","text":text}]
                    })))
                    .into_response()
                }
                _ => axum::Json(serde_json::json!({
                    "jsonrpc":"2.0","id":id,
                    "error":{"code":-32601,"message":"not found"}
                }))
                .into_response(),
            }
        }

        async fn shutdown_handler(State(h): State<H>) -> impl axum::response::IntoResponse {
            h.session.store(true, std::sync::atomic::Ordering::SeqCst);
            "ok"
        }

        let h = H::default();
        let app = axum::Router::new()
            .route("/mcp", post(handler).delete(shutdown_handler))
            .with_state(h);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{}/mcp", addr)
    }

    #[tokio::test]
    async fn test_streamable_http_json_and_sse_round_trip() {
        let url = spawn_http_mock().await;
        let transport = StreamableHttpTransport::new(&url, &HashMap::new()).unwrap();

        // 1. initialize:单 JSON 响应 + 会话 id 捕获
        let init = transport
            .request(
                "initialize",
                serde_json::json!({"protocolVersion":"2024-11-05"}),
            )
            .await
            .expect("initialize should succeed");
        assert_eq!(init["serverInfo"]["name"].as_str(), Some("mock-http"));

        // 2. initialized 通知(无响应体,2xx 即成功)
        transport
            .notify("notifications/initialized", serde_json::json!({}))
            .await
            .expect("notify should succeed");

        // 3. tools/list:SSE 流形态,首个无关事件被跳过,匹配 id 的响应返回
        let tools = transport
            .request("tools/list", serde_json::json!({}))
            .await
            .expect("tools/list via SSE should succeed");
        assert_eq!(tools["tools"][0]["name"].as_str(), Some("echo"));

        // 4. tools/call:单 JSON 回显
        let call = transport
            .request(
                "tools/call",
                serde_json::json!({"name":"echo","arguments":{"message":"hi-http"}}),
            )
            .await
            .expect("tools/call should succeed");
        assert_eq!(call["content"][0]["text"].as_str(), Some("hi-http"));

        // 5. close:DELETE 会话终止
        transport.close().await.expect("close should succeed");
    }

    #[tokio::test]
    async fn test_streamable_http_url_validation() {
        assert!(StreamableHttpTransport::new("", &HashMap::new()).is_err());
        assert!(StreamableHttpTransport::new("ftp://x", &HashMap::new()).is_err());
        assert!(StreamableHttpTransport::new("http://127.0.0.1:1/mcp", &HashMap::new()).is_ok());
    }

    #[test]
    fn test_expand_env_refs() {
        std::env::set_var("B23_TEST_TOKEN", "secret-value");
        assert_eq!(
            expand_env_refs("Bearer ${B23_TEST_TOKEN}").unwrap(),
            "Bearer secret-value"
        );
        assert_eq!(expand_env_refs("plain").unwrap(), "plain");
        assert!(expand_env_refs("${B23_TEST_MISSING_VAR_X}").is_err());
        assert!(expand_env_refs("unclosed ${VAR").is_err());
        assert!(expand_env_refs("${}").is_err());
    }
}
