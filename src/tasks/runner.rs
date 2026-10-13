// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 任务运行器:无 TTY 直接 spawn(tokio::process),stdout/stderr 分捕合流,
//! 两段式取消(优雅→5s→强杀树)。
//!
//! 与 PTY(09 号,交互导向)是两条通道:任务=结果导向(解析/跳转/诊断需要
//! 稳定文本流),无 TTY 让 CLI 自动关色、编码可控;交互任务由前端逃生门文案
//! 引导去集成终端。

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// 优雅取消后等待强杀的窗口
pub const GRACE_KILL_WINDOW: std::time::Duration = std::time::Duration::from_secs(5);

/// 任务定义(tasks.json 简化集,对齐 VS Code 字段名)
#[derive(Debug, Clone, Deserialize)]
pub struct TaskSpec {
    pub label: String,
    #[serde(rename = "type")]
    pub kind: Option<String>, // process(默认) | shell
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub options: TaskOptions,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub problem_matcher: Vec<String>,
    #[serde(rename = "problemMatcher")]
    pub problem_matcher_alias: Option<Vec<String>>,
}

impl TaskSpec {
    /// matcher 集(字段名两读:problemMatcher 驼峰为正式形,problem_matcher 宽容)
    pub fn matchers(&self) -> &[String] {
        self.problem_matcher_alias
            .as_deref()
            .unwrap_or(&self.problem_matcher)
    }

    pub fn is_shell(&self) -> bool {
        self.kind.as_deref() == Some("shell")
    }

    /// 命令原文(确认框/治理留痕用)
    pub fn command_display(&self) -> String {
        if self.is_shell() {
            format!("{} {}", self.command, self.args.join(" "))
        } else {
            let mut parts = vec![self.command.clone()];
            parts.extend(self.args.clone());
            parts.join(" ")
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TaskOptions {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
}

/// cwd 解析与锁定:相对路径基于工作区根;解析后必须仍位于工作区内(防逃逸)
pub fn resolve_cwd(workdir: &Path, option_cwd: Option<&str>) -> Result<PathBuf, String> {
    let base = absolute_dir(workdir)?;
    match option_cwd.filter(|s| !s.trim().is_empty()) {
        None => Ok(base),
        Some(rel) => {
            let joined = if Path::new(rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                base.join(rel)
            };
            let abs = joined
                .canonicalize()
                .map_err(|e| format!("options.cwd 不可达({e}): {rel}"))?;
            let raw = abs.to_string_lossy().to_string();
            let stripped = raw.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or(abs);
            let base_raw = base.to_string_lossy().to_string();
            let base_norm = PathBuf::from(base_raw.strip_prefix(r"\\?\").unwrap_or(&base_raw));
            if stripped.starts_with(&base_norm) {
                Ok(stripped)
            } else {
                Err(format!(
                    "options.cwd 逃逸工作区(拒绝): {rel} → {}",
                    stripped.display()
                ))
            }
        }
    }
}

fn absolute_dir(p: &Path) -> Result<PathBuf, String> {
    let abs = p
        .canonicalize()
        .map_err(|e| format!("工作区根不可达: {e}"))?;
    let raw = abs.to_string_lossy().to_string();
    Ok(raw.strip_prefix(r"\\?\").map(PathBuf::from).unwrap_or(abs))
}

/// spawn 一个任务进程(无 TTY)。输出经 mpsc 逐行外送(stream, line)。
/// 返回进程句柄与行接收端;取消由 [`cancel_two_phase`] 驱动。
pub async fn spawn_task(
    spec: &TaskSpec,
    workdir: &Path,
    line_tx: mpsc::Sender<(String, String)>,
) -> Result<tokio::process::Child, String> {
    let cwd = resolve_cwd(workdir, spec.options.cwd.as_deref())?;
    let mut cmd = Command::new(&spec.command);
    if spec.is_shell() {
        // shell 型:command 为 shell 可执行,args 拼一条命令行(shell -c 形态)
        cmd.args(spec.args.clone());
        // 注:拼接命令行=设计文档明示的注入面,首跑确认框对该形态不可跳过
    } else {
        cmd.args(spec.args.clone());
    }
    cmd.current_dir(&cwd);
    for (k, v) in &spec.options.env {
        cmd.env(k, v);
    }
    // 无 TTY:三流全接,stdin 关闭(交互任务→前端逃生门提示)
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        // 不创建新控制台窗口(无 TTY 语义)
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| format!("任务进程启动失败: {e}"))?;

    // stdout/stderr 分捕→合流(带流标识);UTF-8 lossy(GBK 风险=设计档 §八登记)
    if let Some(out) = child.stdout.take() {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(("stdout".into(), line)).await.is_err() {
                    break;
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let tx = line_tx.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(err).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tx.send(("stderr".into(), line)).await.is_err() {
                    break;
                }
            }
        });
    }
    Ok(child)
}

/// 两段式取消:第一段优雅(Windows=taskkill 无 /F;unix=SIGINT),窗口期后
/// 仍在则第二段强杀树(Windows taskkill /F /T;unix kill -9 进程组+自身)。
pub fn cancel_two_phase(pid: Option<u32>, phase: u32) {
    let Some(pid) = pid else { return };
    if phase <= 1 {
        debug!(pid, "cancel phase 1 (graceful)");
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string()])
                .creation_flags(0x0800_0000)
                .output();
        }
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .output();
        }
    } else {
        warn!(pid, "cancel phase 2 (force kill tree)");
        #[cfg(windows)]
        {
            let _ = std::process::Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(0x0800_0000)
                .output();
        }
        #[cfg(unix)]
        {
            let _ = std::process::Command::new("kill")
                .args(["-9", &format!("-{pid}")])
                .output();
            let _ = std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output();
        }
    }
}

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_locked_inside_workdir() {
        let wd = std::env::current_dir().unwrap();
        // 相对子目录(临时建一个)
        let sub = wd.join(".cwd-lock-probe");
        std::fs::create_dir_all(&sub).unwrap();
        let ok = resolve_cwd(&wd, Some(".cwd-lock-probe"));
        assert!(ok.is_ok());
        std::fs::remove_dir(&sub).ok();
        // 逃逸拒绝
        assert!(resolve_cwd(&wd, Some("../..")).is_err());
        assert!(resolve_cwd(
            &wd,
            Some(if cfg!(windows) { r"C:\Windows" } else { "/etc" })
        )
        .is_err());
    }

    #[test]
    fn command_display_forms() {
        let spec: TaskSpec = serde_json::from_value(serde_json::json!({
            "label": "build", "type": "process", "command": "cargo",
            "args": ["build", "--message-format=short"], "problemMatcher": ["$rustc"]
        }))
        .unwrap();
        assert_eq!(spec.command_display(), "cargo build --message-format=short");
        assert_eq!(spec.matchers(), ["$rustc"]);
        let shell: TaskSpec = serde_json::from_value(serde_json::json!({
            "label": "s", "type": "shell", "command": "echo", "args": ["hi"]
        }))
        .unwrap();
        assert_eq!(shell.command_display(), "echo hi");
    }
}
