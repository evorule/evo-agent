// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! PTY 会话:spawn/读写桥/退出检测/树级 kill。
//!
//! portable-pty 是同步 IO(ConPTY 管道无异步语义)——桥接形态:
//! 专用读线程(阻塞读→tokio 有界通道,满则停读=内核水位反压 shell)+
//! 专用写线程(键盘/resize/kill 单通道复用)+ 专用等退线程(child.wait→退出码)。
//! 会话句柄只暴露通道端;WS 断开经 kill 消息收尾(防孤儿)。
//!
//! shell 发现序(设计档 §3.1 裁决):Windows PowerShell→CMD→Git Bash;
//! unix `$SHELL`→bash→sh;`terminal.shell` 设置覆盖发现序。

#![forbid(unsafe_code)]

use std::path::Path;
use std::sync::mpsc;

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use serde::Serialize;
use tokio::sync::mpsc as tokio_mpsc;
use tracing::debug;

/// 键盘写粒度上限(防畸形帧内存攻击;WS 帧侧另有上限,此处双保险)
pub const MAX_WRITE_CHUNK: usize = 64 * 1024;

/// 一个已 spawn 的 PTY 会话句柄。
///
/// 通道语义:
/// - `msg_tx`:入向(键盘字节/resize/kill),由 WS 泵投递;
/// - `output_rx`:出向字节流(PTY→浏览器);有界通道,消费停摆时读线程自然
///   停读,内核管道水位反压 shell——后端侧背压;
/// - `exit_rx`:退出码(会话终结,恰发一次)。
pub struct PtySessionHandle {
    /// 入向消息通道(键盘/resize/kill)
    pub msg_tx: mpsc::Sender<PtyMsg>,
    /// 出向字节流(PTY→浏览器;有界=背压)
    pub output_rx: tokio_mpsc::Receiver<Vec<u8>>,
    /// 退出码(恰一次)
    pub exit_rx: tokio_mpsc::Receiver<i32>,
    /// shell 进程 id(kill/日志)
    pub pid: Option<u32>,
    /// shell 标签(治理事件)
    pub shell_label: String,
}

/// 入向消息(单通道复用,避免双通道 select)
pub enum PtyMsg {
    /// 键盘字节(透传 shell)
    Input(Vec<u8>),
    /// 终端尺寸变更
    Resize {
        /// 列数
        cols: u16,
        /// 行数
        rows: u16,
    },
    /// 终止会话(树级)
    Kill,
}

/// shell 发现结果
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ShellChoice {
    /// 可执行名(PATH 发现)
    pub program: String,
    /// 启动参数
    pub args: Vec<String>,
    /// 展示标签(治理事件)
    pub label: String,
}

/// shell 发现序(纯函数可测;`override_shell` 为 `terminal.shell` 设置值,
/// 非 "auto" 时按可执行路径直接采用)。
pub fn discover_shell(override_shell: &str) -> ShellChoice {
    if override_shell != "auto" && !override_shell.trim().is_empty() {
        return ShellChoice {
            program: override_shell.trim().to_string(),
            args: vec![],
            label: "custom".to_string(),
        };
    }
    if cfg!(windows) {
        for (program, args, label) in [
            ("powershell.exe", vec!["-NoLogo"], "powershell"),
            ("cmd.exe", vec![], "cmd"),
            ("bash.exe", vec!["-i"], "git-bash"),
        ] {
            if which_program(program) {
                return ShellChoice {
                    program: program.into(),
                    args: args.into_iter().map(String::from).collect(),
                    label: label.into(),
                };
            }
        }
        // 发现全败:交 spawn 报错(错误帧引导)
        ShellChoice {
            program: "powershell.exe".into(),
            args: vec!["-NoLogo".into()],
            label: "powershell".into(),
        }
    } else if let Some(shell) = std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty()) {
        ShellChoice {
            program: shell,
            args: vec![],
            label: "shell".into(),
        }
    } else if which_program("bash") {
        ShellChoice {
            program: "bash".into(),
            args: vec![],
            label: "bash".into(),
        }
    } else {
        ShellChoice {
            program: "sh".into(),
            args: vec![],
            label: "sh".into(),
        }
    }
}

/// PATH 探测(逐目录查文件名;Windows 补 .exe)
fn which_program(program: &str) -> bool {
    let candidates: Vec<String> = if cfg!(windows) {
        let lower = program.to_ascii_lowercase();
        if lower.ends_with(".exe") || lower.ends_with(".cmd") {
            vec![program.to_string()]
        } else {
            vec![format!("{program}.exe")]
        }
    } else {
        vec![program.to_string()]
    };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            for c in &candidates {
                if dir.join(c).is_file() {
                    return true;
                }
            }
        }
    }
    false
}

/// 树级 kill(设计档 §3.1:WS 断开/serve 退出必杀整树防孤儿)。
/// Windows=`taskkill /PID <pid> /T /F`(树级强杀);unix=`kill -9 -<pid>`
/// (进程组;shell 由 PTY 会话首进程担任)。pid 未知时无操作(进程已先亡)。
fn kill_tree_by_pid(pid: Option<u32>) {
    let Some(pid) = pid else { return };
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

/// spawn PTY 会话。
///
/// 尺寸来自前端 fit 实测(ConPTY 零尺寸已知坑);env 注入 TERM;cwd=工作区根。
pub fn spawn_session(
    shell: &ShellChoice,
    workdir: &Path,
    cols: u16,
    rows: u16,
) -> std::io::Result<PtySessionHandle> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    let mut cmd = CommandBuilder::new(shell.program.clone());
    for a in &shell.args {
        cmd.arg(a);
    }
    cmd.cwd(workdir);
    cmd.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let pid = child.process_id();

    // 读线程:阻塞读→tokio 有界通道(满则停读=后端背压)
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let (output_tx, output_rx) = tokio_mpsc::channel::<Vec<u8>>(128);
    std::thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                            break; // 消费端已消失
                        }
                    }
                    Err(e) => {
                        // 平台差异:Linux 关闭后 EIO / Windows 断管均属正常收尾
                        debug!("pty reader ended: {e}");
                        break;
                    }
                }
            }
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // 写线程:入向消息单通道复用(键盘写/master.resize/kill)
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let master: Box<dyn MasterPty + Send> = pair.master;
    let (msg_tx, msg_rx) = mpsc::channel::<PtyMsg>();
    let (exit_tx, exit_rx) = tokio_mpsc::channel::<i32>(1);

    std::thread::Builder::new()
        .name("pty-writer".into())
        .spawn(move || {
            for msg in msg_rx {
                match msg {
                    PtyMsg::Input(data) => {
                        if data.len() > MAX_WRITE_CHUNK {
                            continue;
                        }
                        use std::io::Write as _;
                        if let Err(e) = writer.write_all(&data).and_then(|_| writer.flush()) {
                            debug!("pty write ended: {e}");
                            break;
                        }
                    }
                    PtyMsg::Resize { cols, rows } => {
                        // 「resize a pty that has already exited」竞态:吞
                        if let Err(e) = master.resize(PtySize {
                            rows,
                            cols,
                            pixel_width: 0,
                            pixel_height: 0,
                        }) {
                            debug!("pty resize failed (likely exited): {e}");
                        }
                    }
                    PtyMsg::Kill => {
                        kill_tree_by_pid(pid);
                        break;
                    }
                }
            }
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    // 等退线程:拿退出码→通道(恰一次;kill 路径亦经此收尾)
    std::thread::Builder::new()
        .name("pty-waiter".into())
        .spawn(move || {
            let code = child.wait().map(|s| s.exit_code() as i32).unwrap_or(-1);
            let _ = exit_tx.blocking_send(code);
        })
        .map_err(|e| std::io::Error::other(e.to_string()))?;

    Ok(PtySessionHandle {
        msg_tx,
        output_rx,
        exit_rx,
        pid,
        shell_label: shell.label.clone(),
    })
}

use std::io::Read as _;

#[cfg(windows)]
use std::os::windows::process::CommandExt as _;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_discovery_override_wins() {
        let s = discover_shell("C:\\Program Files\\Git\\bin\\bash.exe");
        assert_eq!(s.program, "C:\\Program Files\\Git\\bin\\bash.exe");
        assert_eq!(s.label, "custom");
        let s = discover_shell("  ");
        assert_ne!(s.label, "custom"); // 空白视同 auto
    }

    #[cfg(windows)]
    #[test]
    fn shell_discovery_windows_order() {
        let s = discover_shell("auto");
        // Windows CI 必有 PowerShell
        assert_eq!(s.label, "powershell");
        assert_eq!(s.program, "powershell.exe");
    }

    #[cfg(unix)]
    #[test]
    fn shell_discovery_unix_fallback() {
        let s = discover_shell("auto");
        assert!(["shell", "bash", "sh"].contains(&s.label.as_str()));
    }

    #[test]
    fn write_chunk_cap_constant() {
        assert_eq!(MAX_WRITE_CHUNK, 64 * 1024);
    }

    /// 真实 PTY 全链:spawn→写命令→读到回显→kill→退出码到位。
    #[test]
    fn pty_full_chain_echo_and_kill() {
        let shell = if cfg!(windows) {
            ShellChoice {
                program: "cmd.exe".into(),
                args: vec![],
                label: "cmd".into(),
            }
        } else {
            ShellChoice {
                program: "sh".into(),
                args: vec![],
                label: "sh".into(),
            }
        };
        let handle = spawn_session(&shell, Path::new("."), 80, 24)
            .expect("pty spawn should work on CI runners");
        let PtySessionHandle {
            msg_tx,
            mut output_rx,
            mut exit_rx,
            pid,
            ..
        } = handle;

        let line = if cfg!(windows) {
            "echo E2E_PROBE_821\r\n"
        } else {
            "echo E2E_PROBE_821\n"
        };
        msg_tx
            .send(PtyMsg::Input(line.as_bytes().to_vec()))
            .unwrap();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let found = rt.block_on(async {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
            let mut acc: Vec<u8> = Vec::new();
            let mut dsr_answered = false;
            loop {
                match tokio::time::timeout_at(deadline, output_rx.recv()).await {
                    Ok(Some(chunk)) => {
                        acc.extend_from_slice(&chunk);
                        // 应答 ConPTY 的 DSR(光标位置查询):真实终端由 xterm.js
                        // 自动应答;裸测试须自答,否则 ConPTY 不绘制
                        if !dsr_answered && acc.windows(4).any(|w| w == b"[6n") {
                            dsr_answered = true;
                            msg_tx.send(PtyMsg::Input(b"[1;1R".to_vec())).ok();
                        }
                        // 连续静默 800ms 视为输出落定
                        if acc.windows(13).any(|w| w == b"E2E_PROBE_821") {
                            return true;
                        }
                    }
                    _ => return false,
                }
            }
        });
        assert!(found, "echo marker not observed in PTY output");

        msg_tx.send(PtyMsg::Kill).ok();
        let code = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10), exit_rx.recv())
                .await
                .ok()
                .flatten()
        });
        assert!(code.is_some(), "exit code should arrive after kill");
        let _ = pid;
    }
}
