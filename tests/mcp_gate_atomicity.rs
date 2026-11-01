// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! MCP 装载门拒载路径的失败原子性测试（spawn 约束设计「必测项」）
//!
//! 断言语义（对齐 TCB 半成品纪律）：enforce 档位下未登记 server 被装载门
//! 拒绝后，agent 状态不得出现该 server 的任何半初始化残留——
//! ① 连接池：manager 快照不含该 server（gate 是 connect 的第一条语句，
//!    拒载发生在任何状态落账之前）；
//! ② 工具注册表 / ③ 能力清单：注册通路在 spawn 之前被门拦下，
//!    `mcp_{server}_{tool}` 形态的工具一个都不出现。
//!
//! 独立集成测试进程的理由：spawn 策略是进程级 OnceLock（单测进程 pin warn
//! 以保证既有 MCP 测试语料的确定性），本测试需要真 enforce 档位走完整
//! 生产路径——`std::env::set_var` 在独立测试二进制内无跨测试竞态。

#![allow(clippy::unwrap_used)]

use evo_agent::config::{McpConfig, McpServerConfig};
use evo_agent::io_handlers::tool_handler::ToolHandler;
use evo_agent::mcp::manager::McpManager;
use evo_agent::mcp::register_mcp_tools;

/// 必然不在任何白名单上的命令名（测试进程未配置治理源 → 本地文件/缺省名单
/// 判定，enforce 下未登记即拒载）
const UNLISTED_COMMAND: &str = "zzz-unlisted-for-atomicity-test-9f3a";

fn probe_cfg() -> McpServerConfig {
    McpServerConfig {
        name: "atomicity-probe".to_string(),
        command: UNLISTED_COMMAND.to_string(),
        args: vec!["--probe".to_string()],
        env: Default::default(),
        disabled: false,
        timeout_secs: 60,
        always_allow: vec![],
        transport: None,
        url: None,
        headers: None,
    }
}

#[tokio::test]
async fn gate_rejection_leaves_no_half_initialized_state() {
    // 真 enforce 档位（独立进程内设置，无其它测试竞争该 OnceLock）
    std::env::set_var("EVORULE_MCP_SPAWN_POLICY", "enforce");
    std::env::remove_var("EVORULE_MCP_GOV_URL");
    std::env::remove_var("EVORULE_MCP_GOV_PROPOSE_KEY");

    // ① 连接池：拒载返回 Err，且 manager 快照**完全不含**该 server
    //    （gate 在状态落账前拒绝——比 failed-with-entry 更强的无残留形态）
    let mgr = McpManager::new();
    let err = mgr
        .connect_server(&probe_cfg())
        .await
        .expect_err("unlisted server must be rejected under enforce");
    assert!(err.contains("未在拉起批准清单"), "reject reason: {err}");
    assert!(
        mgr.snapshot().is_empty(),
        "connection pool must carry no trace of the rejected server"
    );

    // ② 工具注册表 + ③ 能力清单：注册通路被门拦下，零工具落地
    let mut handler = ToolHandler::new();
    let config = McpConfig {
        servers: vec![probe_cfg()],
    };
    let connected = register_mcp_tools(&mut handler, &config).await;
    assert_eq!(
        connected, 0,
        "no tool may be registered for a rejected server"
    );
    let leaked: Vec<String> = handler
        .tool_names()
        .into_iter()
        .filter(|n| n.starts_with("mcp_"))
        .collect();
    assert!(
        leaked.is_empty(),
        "capability surface must carry no trace of the rejected server: {leaked:?}"
    );
}
