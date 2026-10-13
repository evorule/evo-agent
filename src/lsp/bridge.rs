// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! LSP WS↔stdio 桥:双向泵 + 出向拦截 + 生命周期治理。
//!
//! 桥本体与传输载体解耦:出/入两侧都是内存通道(`mpsc`),WS 层(axum upgrade)
//! 只做字符串搬运——因此泵逻辑可用 tokio duplex 内存管道做全链集成测试,
//! 不依赖任何真实语言服务器二进制。
//!
//! 出向拦截(仅两处,其余全保真透传):
//! - `window/logMessage` → 不透传,转日志事件(前端输出通道呈现);
//! - `textDocument/publishDiagnostics` → `params.uri`(file://)换算工作区相对
//!   路径,以 `evoPath` 字段附加后透传(工作区外 URI 附加 null,前端灰置)。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::{io::AsyncWriteExt, sync::mpsc};
use tracing::{debug, warn};

use super::process::LspServerProcess;
use super::transport::{encode_frame, FrameReader};
use super::{Lang, RestartDecision, RestartPolicy};

/// 单帧写 stdin 的超时(子进程卡死时兜底,超时视为写失败)
const STDIN_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// 桥出向事件(WS 层转成文本帧:`Text` 原样,其余转 `{"type":"lsp_*"}` 控制帧)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeEvent {
    /// LSP JSON-RPC 帧(已按拦截规则处理)
    Text(String),
    /// 生命周期状态(`starting`/`restarting`/`stopped`/`failed`)
    State {
        /// 状态名
        state: &'static str,
        /// 人读详情(日志/横幅展示)
        detail: String,
    },
    /// 语言服务器日志分流(window/logMessage;`level`: error/warning/info/log)
    Log {
        /// 日志级别名
        level: String,
        /// 日志正文
        message: String,
    },
    /// 服务器不可用(spawn 失败:不在 PATH 等;前端灰置降级)
    Unavailable {
        /// 安装引导文案
        hint: String,
    },
}

/// 出向帧处置结果(拦截判定,纯函数可单测)
#[derive(Debug, PartialEq, Eq)]
pub enum OutAction {
    /// 原样(或注 evoPath 后)转发给客户端
    Forward(String),
    /// 分流为日志事件,不透传
    Log {
        /// 日志级别名
        level: String,
        /// 日志正文
        message: String,
    },
    /// 丢弃(非 JSON 等无效帧)
    Ignore,
}

/// 会话结束原因
#[derive(Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// 客户端 WS 关闭(调用方 kill 子进程后整体退出)
    ClientClosed,
    /// 服务进程退出(回到重启决策)
    ServerExited,
}

impl SessionEnd {}

/// 出向帧拦截:JSON 解析失败丢弃(防注水);logMessage 分流;publishDiagnostics
/// 附加 evoPath;其余原样透传。
pub fn process_out_frame(body: &[u8], workdir: &Path) -> OutAction {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        debug!("dropping non-JSON frame from language server");
        return OutAction::Ignore;
    };
    match value.get("method").and_then(|m| m.as_str()) {
        Some("window/logMessage") => OutAction::Log {
            level: log_level_name(
                value
                    .pointer("/params/type")
                    .and_then(|t| t.as_u64())
                    .unwrap_or(4),
            ),
            message: value
                .pointer("/params/message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        },
        Some("textDocument/publishDiagnostics") => {
            let mut value = value;
            let evo_path = value
                .pointer("/params/uri")
                .and_then(|u| u.as_str())
                .and_then(|u| uri_to_evo_path(u, workdir));
            if let Some(params) = value.pointer_mut("/params") {
                params["evoPath"] = evo_path.map(Value::String).unwrap_or(Value::Null);
            }
            OutAction::Forward(value.to_string())
        }
        _ => OutAction::Forward(value.to_string()),
    }
}

/// LSP logMessage type(1=Error 2=Warning 3=Info 4=Log)
fn log_level_name(t: u64) -> String {
    match t {
        1 => "error",
        2 => "warning",
        3 => "info",
        _ => "log",
    }
    .to_string()
}

/// file:// URI → 工作区相对路径(POSIX 风格 `/`,与 file_api/openFile 同一约定);
/// 非 file scheme 或不在工作区内返回 None(前端按未知路径灰置)。
pub fn uri_to_evo_path(uri: &str, workdir: &Path) -> Option<String> {
    let parsed = url::Url::parse(uri).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    let abs = parsed.to_file_path().ok()?;
    let rel = abs.strip_prefix(workdir).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// 客户端→服务器入向帧:识别 shutdown(正常停止语义)后编码透传。
/// 返回 false = 写 stdin 失败(进程已死;由 stdout EOF 路径收尾)。
async fn handle_in_frame(stdin: &mut (impl tokio::io::AsyncWrite + Unpin), text: &str) -> bool {
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        if v.get("method").and_then(|m| m.as_str()) == Some("shutdown") {
            debug!("client requested shutdown");
        }
    }
    let frame = encode_frame(text.as_bytes());
    match tokio::time::timeout(STDIN_WRITE_TIMEOUT, stdin.write_all(&frame)).await {
        Ok(Ok(())) => stdin.flush().await.is_ok(),
        Ok(Err(e)) => {
            debug!(error = %e, "stdin write failed");
            false
        }
        Err(_) => {
            warn!("stdin write timed out; language server considered wedged");
            false
        }
    }
}

/// 单会话泵:一个已 spawn 的服务进程,直到客户端关闭或服务端退出。
///
/// `stdin`/`stdout` 由调用方注入(生产=子进程管道;测试=duplex 内存管道)。
/// stdin 写坏后转入纯排空模式(丢上行、保下行),直至客户端关闭或 stdout EOF。
pub async fn pump_session<S, W>(
    stdin: &mut W,
    stdout: &mut FrameReader<S>,
    incoming: &mut mpsc::Receiver<String>,
    outgoing: &mut mpsc::Sender<BridgeEvent>,
    workdir: &Path,
) -> SessionEnd
where
    S: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut stdin_broken = false;
    loop {
        tokio::select! {
            text = incoming.recv() => match text {
                Some(t) => {
                    if stdin_broken {
                        // 排空模式:丢上行保下行,收尾交 stdout EOF
                        continue;
                    }
                    if !handle_in_frame(stdin, &t).await {
                        // 写失败:进程大概率已死。转排空模式
                        // (嵌套 await 会饿死 select 的另一分支,不可取)
                        stdin_broken = true;
                    }
                }
                None => {
                    let _ = outgoing
                        .send(BridgeEvent::State {
                            state: "stopped",
                            detail: "client disconnected".into(),
                        })
                        .await;
                    return SessionEnd::ClientClosed;
                }
            },
            frame = stdout.next_frame() => match frame {
                Ok(body) => match process_out_frame(&body, workdir) {
                    OutAction::Forward(text) => {
                        if outgoing.send(BridgeEvent::Text(text)).await.is_err() {
                            return SessionEnd::ClientClosed;
                        }
                    }
                    OutAction::Log { level, message } => {
                        let _ = outgoing.send(BridgeEvent::Log { level, message }).await;
                    }
                    OutAction::Ignore => {}
                },
                // stdout EOF/畸形帧:服务进程退出(或输出流断)
                Err(e) => {
                    debug!(error = %e, "language server stdout ended");
                    return SessionEnd::ServerExited;
                }
            },
        }
    }
}

/// 桥主循环:spawn → 泵 → 进程退出 → 退避重启(60s 窗口 3 次)或终态。
///
/// 客户端关闭(ClientClosed)时 kill 子进程后返回;服务器退出走重启决策,
/// 预算耗尽推 `failed` 终态(附 stderr 尾部现场)。
pub async fn run_bridge(
    lang: Lang,
    workdir: PathBuf,
    mut incoming: mpsc::Receiver<String>,
    mut outgoing: mpsc::Sender<BridgeEvent>,
) {
    let spec = lang.spec();
    let wd = canonicalized_workdir(&workdir);
    let mut policy = RestartPolicy::new();

    loop {
        let mut proc = match LspServerProcess::spawn(&spec, &wd).await {
            Ok(p) => p,
            Err(e) => {
                let hint = format!(
                    "{} 启动失败({e})。安装指引:Rust 门 `rustup component add rust-analyzer`;TypeScript 门 `npm install -g typescript typescript-language-server`",
                    spec.program
                );
                let _ = outgoing.send(BridgeEvent::Unavailable { hint }).await;
                return;
            }
        };
        let _ = outgoing
            .send(BridgeEvent::State {
                state: "starting",
                detail: format!("{} spawned (pid {:?})", spec.program, proc.pid),
            })
            .await;

        // pump 持 stdin/stdout 可变借用,返回后借小孩与 stderr 现场收尸
        let pid = proc.pid;
        let end = pump_session(
            &mut proc.stdin,
            &mut proc.stdout,
            &mut incoming,
            &mut outgoing,
            &wd,
        )
        .await;

        match end {
            SessionEnd::ClientClosed => {
                proc.child.start_kill().ok();
                let _ = proc.child.wait().await;
                debug!(pid, "bridge closed by client");
                return;
            }
            SessionEnd::ServerExited => {
                let status = proc.child.wait().await;
                if incoming_terminated(&mut incoming).await {
                    let _ = outgoing
                        .send(BridgeEvent::State {
                            state: "stopped",
                            detail: "client disconnected".into(),
                        })
                        .await;
                    return;
                }
                let stderr_tail = proc.stderr_ring.snapshot();
                match policy.record_exit(Instant::now()) {
                    RestartDecision::Retry(backoff) => {
                        let attempt = policy.restarts_in_window();
                        warn!(
                            pid,
                            backoff_secs = backoff.as_secs(),
                            "language server exited unexpectedly; restarting"
                        );
                        let _ = outgoing
                            .send(BridgeEvent::State {
                                state: "restarting",
                                detail: format!(
                                    "{program} 已退出({status}),{backoff_secs}s 后第 {attempt}/{max} 次重启",
                                    program = spec.program,
                                    status = status.map(|s| s.to_string()).unwrap_or_default(),
                                    backoff_secs = backoff.as_secs(),
                                    attempt = attempt,
                                    max = 3,
                                ),
                            })
                            .await;
                        tokio::time::sleep(backoff).await;
                        // 客户端可能在退避期断开
                        if incoming_terminated(&mut incoming).await {
                            let _ = outgoing
                                .send(BridgeEvent::State {
                                    state: "stopped",
                                    detail: "client disconnected".into(),
                                })
                                .await;
                            return;
                        }
                    }
                    RestartDecision::Exhausted => {
                        warn!(pid, "language server restart budget exhausted");
                        let detail = if stderr_tail.is_empty() {
                            format!(
                                "{program} 连续崩溃,重启预算耗尽;请检查服务器安装与工作区规模",
                                program = spec.program
                            )
                        } else {
                            format!(
                                "{program} 连续崩溃,重启预算耗尽。stderr 尾部:\n{stderr_tail}",
                                program = spec.program
                            )
                        };
                        let _ = outgoing
                            .send(BridgeEvent::State {
                                state: "failed",
                                detail,
                            })
                            .await;
                        return;
                    }
                }
            }
        }
    }
}

/// 非阻塞探测客户端通道是否已关闭(退避期/收尾路径用)。
fn incoming_terminated(incoming: &mut mpsc::Receiver<String>) -> impl Future<Output = bool> + '_ {
    use tokio::sync::mpsc::error::TryRecvError;
    async move { matches!(incoming.try_recv(), Err(TryRecvError::Disconnected)) }
}

use std::future::Future;

/// 工作区根规整:绝对化(相对 workdir 如 "." 无法做 URI 前缀截取)并剥除
/// Windows canonicalize 的 `\\?\` verbatim 前缀——语言服务器推送的 file:// URI
/// 还原出的路径是非 verbatim 形态,前缀不齐则 evoPath 换算恒失败
/// (fs_watch 同族教训;UNC `\\?\UNC\` 形态本项目工作区不涉及,不做展开)。
fn canonicalized_workdir(workdir: &Path) -> PathBuf {
    let abs = workdir
        .canonicalize()
        .unwrap_or_else(|_| workdir.to_path_buf());
    let raw = abs.to_string_lossy().to_string();
    match raw.strip_prefix(r"\\?\") {
        Some(stripped) => PathBuf::from(stripped),
        None => abs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const TEST_WORKDIR: &str = env!("CARGO_MANIFEST_DIR");

    // ---- 拦截判定(纯函数) ----

    #[test]
    fn log_message_intercepted_not_forwarded() {
        let body = br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"indexing 42 files"}}"#;
        match process_out_frame(body, Path::new(TEST_WORKDIR)) {
            OutAction::Log { level, message } => {
                assert_eq!(level, "info");
                assert_eq!(message, "indexing 42 files");
            }
            other => panic!("expected Log, got {other:?}"),
        }
    }

    #[test]
    fn publish_diagnostics_annotated_with_relative_path() {
        let uri = url::Url::from_file_path(Path::new(TEST_WORKDIR).join("src").join("lib.rs"))
            .unwrap()
            .to_string();
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{uri}","version":3,"diagnostics":[]}}}}"#
        );
        match process_out_frame(body.as_bytes(), Path::new(TEST_WORKDIR)) {
            OutAction::Forward(text) => {
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v.pointer("/params/evoPath"), Some(&json!("src/lib.rs")));
                assert_eq!(v.pointer("/params/version"), Some(&json!(3)));
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn outside_workdir_uri_annotated_null() {
        let uri = url::Url::from_file_path(if cfg!(windows) {
            r"C:\elsewhere\x.rs"
        } else {
            "/elsewhere/x.rs"
        })
        .unwrap()
        .to_string();
        let body = format!(
            r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{uri}","diagnostics":[]}}}}"#
        );
        match process_out_frame(body.as_bytes(), Path::new(TEST_WORKDIR)) {
            OutAction::Forward(text) => {
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v.pointer("/params/evoPath"), Some(&Value::Null));
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn non_file_scheme_diagnostics_annotated_null() {
        let body = br#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"untitled:Untitled-1","diagnostics":[]}}"#;
        match process_out_frame(body, Path::new(TEST_WORKDIR)) {
            OutAction::Forward(text) => {
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v.pointer("/params/evoPath"), Some(&Value::Null));
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn other_methods_passthrough_verbatim() {
        let body = br#"{"jsonrpc":"2.0","id":7,"method":"textDocument/completion","params":{}}"#;
        match process_out_frame(body, Path::new(TEST_WORKDIR)) {
            OutAction::Forward(text) => {
                let v: Value = serde_json::from_str(&text).unwrap();
                assert_eq!(v["id"], json!(7));
            }
            other => panic!("expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn non_json_frame_dropped() {
        assert_eq!(
            process_out_frame(b"not-json", Path::new(TEST_WORKDIR)),
            OutAction::Ignore
        );
    }

    #[test]
    fn canonicalized_workdir_strips_verbatim_and_absolutizes() {
        let wd = canonicalized_workdir(Path::new("."));
        assert!(wd.is_absolute());
        let raw = wd.to_string_lossy();
        assert!(
            !raw.contains(r"\?\"),
            "verbatim prefix must be stripped: {raw}"
        );
        // URI 还原路径可直接前缀截取(strip_prefix 语义等价)
        let p = wd.join("src").join("main.rs");
        assert!(p.strip_prefix(&wd).is_ok());
    }

    #[test]
    fn uri_to_evo_path_posix_style() {
        let p = Path::new(TEST_WORKDIR).join("web").join("src");
        let uri = url::Url::from_file_path(&p).unwrap().to_string();
        assert_eq!(
            uri_to_evo_path(&uri, Path::new(TEST_WORKDIR)).as_deref(),
            Some("web/src")
        );
        assert_eq!(uri_to_evo_path("http://x/y", Path::new(TEST_WORKDIR)), None);
    }
}

/// 泵全链集成测试(duplex 内存管道,零真实二进制依赖)。
#[cfg(test)]
mod e2e {
    use super::*;
    use serde_json::json;
    use tokio::io::{duplex, split, AsyncReadExt, AsyncWriteExt};

    const TEST_WORKDIR: &str = env!("CARGO_MANIFEST_DIR");

    async fn read_frame<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Vec<u8> {
        let mut header = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            r.read_exact(&mut byte).await.unwrap();
            header.push(byte[0]);
            if header.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        let header_str = String::from_utf8(header).unwrap();
        let len: usize = header_str
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await.unwrap();
        body
    }

    #[tokio::test]
    async fn full_chain_initialize_diagnostics_log_split() {
        // 一条 duplex:桥端与 mock LS 端各执一端
        let (bridge_end, ls_end) = duplex(65536);
        let (b_read, mut b_write) = split(bridge_end);
        let (mut a_read, mut a_write) = split(ls_end);

        let pub_uri =
            url::Url::from_file_path(Path::new(TEST_WORKDIR).join("src").join("main.rs")).unwrap();
        let pub_uri2 = pub_uri.clone();

        // mock LS:收 initialize → 回响应 → 推诊断+日志 → 收 didOpen → 关闭输出
        let ls = tokio::spawn(async move {
            let init = read_frame(&mut a_read).await;
            let v: Value = serde_json::from_slice(&init).unwrap();
            assert_eq!(v["method"], "initialize");
            a_write
                .write_all(&encode_frame(
                    br#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}"#,
                ))
                .await
                .unwrap();
            let notif = format!(
                r#"{{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{{"uri":"{pub_uri2}","version":1,"diagnostics":[{{"range":{{"start":{{"line":2,"character":4}},"end":{{"line":2,"character":9}}}},"message":"mock err","severity":1,"source":"mock"}}]}}}}"#
            );
            a_write
                .write_all(&encode_frame(notif.as_bytes()))
                .await
                .unwrap();
            a_write
                .write_all(&encode_frame(
                    br#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":2,"message":"cache warm"}}"#,
                ))
                .await
                .unwrap();
            let _did_open = read_frame(&mut a_read).await;
            a_write.shutdown().await.unwrap();
            drop(a_write);
        });

        let (in_tx, mut in_rx) = mpsc::channel::<String>(16);
        let (mut out_tx, mut out_rx) = mpsc::channel::<BridgeEvent>(16);

        let mut frame_reader = FrameReader::new(b_read);
        let pump = tokio::spawn(async move {
            pump_session(
                &mut b_write,
                &mut frame_reader,
                &mut in_rx,
                &mut out_tx,
                Path::new(TEST_WORKDIR),
            )
            .await
        });

        // 客户端发 initialize → LS 应收到
        in_tx
            .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#.into())
            .await
            .unwrap();

        // 桥转发序:initialize 响应(Text) → 诊断(Text+evoPath) → 日志(Log 事件)
        let mut saw_response = false;
        let mut saw_diag_with_path = false;
        let mut saw_log_split = false;
        for _ in 0..3 {
            let ev = tokio::time::timeout(std::time::Duration::from_secs(5), out_rx.recv())
                .await
                .expect("timeout waiting bridge event")
                .expect("bridge ended early");
            match ev {
                BridgeEvent::Text(t) => {
                    let v: Value = serde_json::from_str(&t).unwrap();
                    if v.get("result").is_some() {
                        saw_response = true;
                    }
                    if v.get("method").and_then(|m| m.as_str())
                        == Some("textDocument/publishDiagnostics")
                    {
                        assert_eq!(v.pointer("/params/evoPath"), Some(&json!("src/main.rs")));
                        assert_eq!(v.pointer("/params/diagnostics/0/severity"), Some(&json!(1)));
                        saw_diag_with_path = true;
                    }
                }
                BridgeEvent::Log { level, message } => {
                    assert_eq!(level, "warning");
                    assert_eq!(message, "cache warm");
                    saw_log_split = true;
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
        assert!(saw_response, "initialize response missing");
        assert!(saw_diag_with_path, "annotated diagnostics missing");
        assert!(saw_log_split, "log split missing");

        // 客户端发 didOpen → mock LS 收到;随后客户端关闭 → ClientClosed
        in_tx
            .send(r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{}}"#.into())
            .await
            .unwrap();
        drop(in_tx);
        let end = pump.await.unwrap();
        assert_eq!(end, SessionEnd::ClientClosed);
        ls.await.unwrap();
    }

    #[tokio::test]
    async fn server_exit_reports_session_end() {
        // LS 端直接关闭输出流 → stdout EOF → ServerExited
        let (bridge_end, mut ls_end) = duplex(65536);
        let (b_read, mut b_write) = split(bridge_end);
        let (in_tx, mut in_rx) = mpsc::channel::<String>(16);
        let (mut out_tx, _out_rx) = mpsc::channel::<BridgeEvent>(16);
        let mut frame_reader = FrameReader::new(b_read);
        ls_end.shutdown().await.unwrap();
        drop(ls_end);
        let pump = tokio::spawn(async move {
            pump_session(
                &mut b_write,
                &mut frame_reader,
                &mut in_rx,
                &mut out_tx,
                Path::new(TEST_WORKDIR),
            )
            .await
        });
        let _ = in_tx.send("{}".into()).await;
        let end = tokio::time::timeout(std::time::Duration::from_secs(5), pump)
            .await
            .expect("pump should end on stdout EOF")
            .unwrap();
        assert_eq!(end, SessionEnd::ServerExited);
    }
}
