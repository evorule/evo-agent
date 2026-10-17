// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! DAP（Debug Adapter Protocol）桥——运行和调试板块的 serve 端门。
//!
//! 形态与 [`crate::lsp`] 语言门同构：WS 帧内的 JSON 文本 ↔ 适配器进程 stdio
//! 的 Content-Length 帧双向透传；生命周期与 WS 连接绑定（断连即 kill 适配器，
//! 无自动重启——调试会话不中途复活）。与 LSP 门的两点差异：
//! - 适配器清单来自设置键 `dap.adapters`（**fail-closed 缺省空**——部署方
//!   显式配置后才可 spawn），而非代码内置候选；
//! - 无 initialize 语义加工：DAP 消息原样透传（含 frontend 的 initialize/
//!   launch/setBreakpoints 全协商），桥不理解内容只搬运。
//!
//! 治理：spawn/exit 记录进治理事件流（record-only，`dap_session` 事件型）；
//! WS 鉴权与既有 WS 端点同款双层（auth 中间件 + `?token=` fallback）。

use std::path::PathBuf;

use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// 白名单条目（设置键 `dap.adapters` 的对象值元素）
#[derive(Debug, Clone, serde::Deserialize)]
pub struct AdapterSpec {
    /// 适配器可执行文件（绝对路径或 PATH 内命令名）
    pub command: String,
    /// 启动参数（逐元素透传，不经 shell）
    #[serde(default)]
    pub args: Vec<String>,
    /// 适配器声明支持的语言标签（前端呈现用；门不复核）
    #[serde(default)]
    pub langs: Vec<String>,
}

/// 从设置值解析适配器映射（`{ "名称": {command,args,langs} }`）。
///
/// 设置注册表把 `dap.adapters` 声明为 **String** 形态（默认 "{}"，JSON 文本），
/// 而本函数早期只认对象形态——白名单经 PUT 写入后永远解析为空（真缺陷，
/// 2026-10-17 复检发现）。现兼容两形态：字符串 → 解析内嵌 JSON；对象 → 直用。
pub fn adapters_from_settings(v: Option<&Value>) -> Vec<(String, AdapterSpec)> {
    let raw = match v {
        None => return Vec::new(),
        Some(Value::String(s)) => {
            // 字符串形态:解析内嵌 JSON;解析失败/非对象 = 空（保守不 spawn）
            match serde_json::from_str::<Value>(s) {
                Ok(parsed) => parsed,
                Err(_) => return Vec::new(),
            }
        }
        Some(other) => other.clone(),
    };
    let Some(map) = raw.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, spec) in map {
        match serde_json::from_value::<AdapterSpec>(spec.clone()) {
            Ok(s) => out.push((name.clone(), s)),
            Err(_) => continue, // 形状不符的条目跳过（保守不 spawn）
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// 适配器进程（透传泵持有 stdin/stdout；stderr 丢弃——DAP 无 stderr 协议语义）
pub struct AdapterProcess {
    pub pid: Option<u32>,
    stdin: tokio::process::ChildStdin,
    stdout: crate::lsp::transport::FrameReader<tokio::process::ChildStdout>,
    child: tokio::process::Child,
}

impl AdapterProcess {
    /// spawn 适配器（工作区目录起；不经 shell；Windows 抑制控制台窗口）
    pub async fn spawn(spec: &AdapterSpec, workdir: &PathBuf) -> std::io::Result<Self> {
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .current_dir(workdir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        #[cfg(windows)]
        {
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("adapter '{}' spawn failed: {e}", spec.command),
            )
        })?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| std::io::Error::other("child stdin unavailable"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout unavailable"))?;
        Ok(Self {
            pid,
            stdin,
            stdout: crate::lsp::transport::FrameReader::new(stdout),
            child,
        })
    }

    pub fn kill(&mut self) {
        self.child.start_kill().ok();
    }
}

/// 桥会话终态
pub enum SessionEnd {
    /// WS 客户端断开（适配器 kill 收尾）
    ClientClosed,
    /// 适配器退出（stdout EOF）
    AdapterExited,
}

/// WS 上行文本帧 → 适配器 stdin（Content-Length 加帧）。返回 false = 写失败。
async fn write_in_frame(stdin: &mut tokio::process::ChildStdin, text: &str) -> bool {
    let frame = crate::lsp::transport::encode_frame(text.as_bytes());
    stdin.write_all(&frame).await.is_ok()
}

/// 桥主循环：双向透传直至任一侧关闭。
///
/// - 入向（WS 文本帧）：逐帧加 Content-Length 头写适配器 stdin；
/// - 出向（适配器 stdout 帧）：解析一帧 → 正文 JSON 作为 WS 文本帧转发；
/// - 适配器 stderr 已 null（DAP 无 stderr 协议语义；异常输出走适配器自己的
///   output 事件）。
pub async fn run_bridge(
    spec: AdapterSpec,
    workdir: PathBuf,
    mut incoming: mpsc::Receiver<String>,
    mut outgoing: mpsc::Sender<String>,
) -> SessionEnd {
    let mut proc = match AdapterProcess::spawn(&spec, &workdir).await {
        Ok(p) => p,
        Err(e) => {
            let _ = outgoing
                .send(format!(
                    "{{\"type\":\"dap_error\",\"message\":\"adapter spawn failed: {}\"}}",
                    e
                ))
                .await;
            return SessionEnd::AdapterExited;
        }
    };
    tracing::info!(pid = proc.pid, "dap adapter spawned");

    loop {
        tokio::select! {
            text = incoming.recv() => match text {
                Some(t) => {
                    if write_in_frame(&mut proc.stdin, &t).await {
                        if let Err(e) = proc.child.try_wait() {
                            let _ = outgoing.send(format!("{{\"type\":\"dap_error\",\"message\":\"{e}\"}}")).await;
                        }
                    } else {
                        return SessionEnd::ClientClosed;
                    }
                }
                None => return SessionEnd::ClientClosed,
            },
            frame = proc.stdout.next_frame() => match frame {
                Ok(body) => {
                    let text = String::from_utf8_lossy(&body).into_owned();
                    if outgoing.send(text).await.is_err() {
                        return SessionEnd::ClientClosed;
                    }
                }
                Err(_) => {
                    // 适配器 stdout EOF = 进程退出
                    let status = proc.child.wait().await.ok().map(|s| s.to_string());
                    tracing::info!(status = ?status, "dap adapter exited");
                    let _ = outgoing
                        .send(format!(
                            "{{\"type\":\"dap_exited\",\"status\":{}}}",
                            status.map_or_else(
                                || "null".to_string(),
                                |s| serde_json::Value::String(s).to_string()
                            )
                        ))
                        .await;
                    return SessionEnd::AdapterExited;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adapters_from_settings_parses_and_skips_malformed() {
        let v = json!({
            "rust": { "command": "lldb-dap", "args": [], "langs": ["rust"] },
            "broken": { "nope": 1 }
        });
        let out = adapters_from_settings(Some(&v));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "rust");
        assert_eq!(out[0].1.command, "lldb-dap");
        // 缺省（None/非对象）= 空（fail-closed）
        assert!(adapters_from_settings(None).is_empty());
        assert!(adapters_from_settings(Some(&json!("x"))).is_empty());
    }

    #[test]
    fn adapters_sorted_by_name_for_stable_listing() {
        let v = json!({
            "zeta": { "command": "z" },
            "alpha": { "command": "a", "args": ["--x"], "langs": ["py"] }
        });
        let out = adapters_from_settings(Some(&v));
        assert_eq!(out[0].0, "alpha");
        assert_eq!(out[1].0, "zeta");
        assert_eq!(out[0].1.args, vec!["--x"]);
    }

    #[test]
    fn adapters_from_string_form_setting_key() {
        // 注册表形态:dap.adapters 为 String(JSON 文本)——对象形态修复前此路径恒空
        let s = json!(r#"{"python": {"command": "py", "args": ["-m", "debugpy.adapter"], "langs": ["python"]}}"#);
        let out = adapters_from_settings(Some(&s));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "python");
        assert_eq!(out[0].1.command, "py");
        assert_eq!(out[0].1.args, vec!["-m", "debugpy.adapter"]);
        // 非法 JSON 文本/非对象 JSON = 空（fail-closed 不变）
        assert!(adapters_from_settings(Some(&json!("not json"))).is_empty());
        assert!(adapters_from_settings(Some(&json!("[1,2]"))).is_empty());
    }
}
