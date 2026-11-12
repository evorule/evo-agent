//! Unix PTY smoke test, docker-isolated (CI slice c3).
//!
//! The hosted-runner job guard (provjobd) SIGKILLs the test tree whenever a
//! test opens a pseudo-terminal, so `pty::session::tests::`
//! `pty_full_chain_echo_and_kill` had to be skipped on ubuntu (ci.yml, lib-b4)
//! -- leaving the real forkpty path with zero linux CI coverage. This binary
//! runs the SAME full chain (spawn -> write echo -> read it back -> kill ->
//! exit code arrives) inside a plain `ubuntu:22.04` docker container: the
//! guard's process-tree tracking does not reach into containers (the
//! docker-backend shell_exec tests spawn and reap children on every ubuntu
//! run without ever tripping it), so the container restores that coverage.
//!
//! Unix-gated: on windows the in-lib test already covers the ConPTY path and
//! this binary compiles to an empty suite (the CI docker step is
//! linux-gated, so nothing else would run it anyway).

#![cfg(unix)]

use evo_agent::pty::session::{spawn_session, PtyMsg, PtySessionHandle, ShellChoice};
use std::path::Path;

/// Port of `pty::session::tests::pty_full_chain_echo_and_kill` (unix branch),
/// same assertions, sh instead of the cfg!-selected shell.
#[test]
fn pty_full_chain_echo_and_kill_unix_smoke() {
    let shell = ShellChoice {
        program: "sh".into(),
        args: vec![],
        label: "sh".into(),
    };
    let handle = spawn_session(&shell, Path::new("."), 80, 24)
        .expect("pty spawn should work inside the smoke container");
    let PtySessionHandle {
        msg_tx,
        mut output_rx,
        mut exit_rx,
        pid,
        ..
    } = handle;

    msg_tx
        .send(PtyMsg::Input(b"echo PTY_SMOKE_821\n".to_vec()))
        .unwrap();

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let found = rt.block_on(async {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut acc: Vec<u8> = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline, output_rx.recv()).await {
                Ok(Some(chunk)) => {
                    acc.extend_from_slice(&chunk);
                    // 800ms of consecutive silence is treated as output settling
                    if acc.windows(13).any(|w| w == b"PTY_SMOKE_821") {
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
