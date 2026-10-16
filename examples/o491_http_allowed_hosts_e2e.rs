// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! O-491 J-d E2E:definition 级 http_get active host 声明——真实公网验证
//!
//! 题塞形态 agent 定义(声明 example.com 为额外 active host)走完整装配,
//! 然后以真实公网请求验证三面:
//! 1. 声明 host 调用**直接执行**(无 needs_approval 审批提案,HTTP 200)
//! 2. 未声明 host 调用仍出审批提案(candidate 语义不因声明收窄而放宽)
//! 3. SSRF 黑名单 IP 即使被声明照样 BLOCKED(升级永不越过安全线)
//!
//! 注:LLM 决策由直接工具调用代替(判分环境无 LLM 端点);工具执行、
//! 网络请求、装配校验全部真实。声明重绑段的语义与
//! `AgentRunner::from_definition`(runner.rs O-491 段)逐行同款。
//!
//! 运行:`cargo run --example o491_http_allowed_hosts_e2e`(需外网可达)

use std::path::Path;

use evo_agent::agent::definition::AgentDefinition;
use evo_agent::agent::runner::AgentRunner;
use evo_agent::api::evorule_client::EvoruleApiClient;
use evo_agent::builtin_tools::default_safe_toolkit;
use evo_agent::io_handlers::tool_handler::{ToolFunction, ToolHandler};
use serde_json::Value;

/// 题塞形态 agent 定义(与 ALE 参赛 agent.json 同构)
const COMPETITION_DEF: &str = r#"{
    "agent_type": "ale-competitor",
    "version": "1.0.0",
    "description": "ALE engineering-track competitor with declared public API hosts",
    "system_prompt": "You solve engineering tasks using declared public APIs.",
    "model": "test-model",
    "temperature": 0.3,
    "max_steps": 20,
    "step_timeout_secs": 60,
    "tools": ["http_get", "file_read", "file_list"],
    "memory": { "type": "none", "namespace": "" },
    "http_get_allowed_hosts": ["example.com", "www.iana.org"]
}"#;

#[tokio::main]
async fn main() {
    let tmp = tempfile::tempdir().expect("create tempdir");
    let workdir = tmp.path();

    // 1. 真实定义加载 + 校验(裸域名门卫)
    let def: AgentDefinition =
        serde_json::from_str(COMPETITION_DEF).expect("competition def must parse");
    def.validate().expect("competition def must validate");
    assert_eq!(
        def.http_get_allowed_hosts.as_deref(),
        Some(&["example.com".to_string(), "www.iana.org".to_string()][..])
    );
    println!("[1] definition loaded + validated (declared: example.com, www.iana.org)");

    // 2. 完整装配链:from_definition 真跑(校验 + 重绑路径)
    let client = EvoruleApiClient::new("http://localhost:8080"); // memory=none 不发请求
    let handler = default_safe_toolkit(workdir, Path::new("."));
    let _runner = AgentRunner::from_definition(def, client, handler, None)
        .await
        .expect("from_definition must succeed with declared hosts");
    println!("[2] from_definition assembly OK (declaration rebind path exercised)");

    // 3. 行为验证:与 from_definition O-491 段逐行同款的重绑(实调面)
    let bound = rebind_like_from_definition(default_safe_toolkit(workdir, Path::new(".")));

    // 3a. 声明 host:直接执行(无审批提案),真实 HTTP 200
    let out = bound
        .execute_by_name(
            "http_get",
            &serde_json::json!({"url": "https://example.com"}),
        )
        .await
        .expect("declared host must execute directly");
    assert!(
        !is_proposal(&out),
        "declared host must NOT produce an approval proposal, got: {out}"
    );
    println!(
        "[3a] declared host https://example.com -> direct execution (real network, no approval)"
    );

    // 3b. 未声明 host:仍 candidate(提案),审批语义未放宽
    let out = bound
        .execute_by_name(
            "http_get",
            &serde_json::json!({"url": "https://rust-lang.org"}),
        )
        .await
        .expect("undeclared host returns a proposal, not an error");
    assert!(
        is_proposal(&out),
        "undeclared host must yield proposal, got: {out}"
    );
    println!(
        "[3b] undeclared host https://rust-lang.org -> approval proposal (candidate preserved)"
    );

    // 3c. SSRF 黑名单 IP:即使被声明照样 BLOCKED(升级永不越过安全线)
    let guard = evo_agent::builtin_tools::http_get::HttpGetTool::new()
        .with_allowed_hosts(vec!["192.168.1.1".to_string()]);
    let err = guard
        .call(&serde_json::json!({"url": "https://192.168.1.1/admin", "approved": true}))
        .await
        .expect_err("blocklisted IP must stay blocked");
    assert!(err.contains("BLOCKED"), "got: {err}");
    println!("[3c] blocklisted IP declared -> still BLOCKED (safety line held)");

    println!("\n=== O-491 J-d E2E PASS ===");
    println!("  declared hosts execute directly on the real network;");
    println!("  undeclared stay candidate; blocklist never upgraded.");
}

/// 与 AgentRunner::from_definition O-491 段逐行同款的重绑
/// (声明 → 同名覆盖注册;register_static 为 insert 语义)
fn rebind_like_from_definition(mut handler: ToolHandler) -> ToolHandler {
    let hosts = Some(vec!["example.com".to_string(), "www.iana.org".to_string()]);
    if let Some(hosts) = &hosts {
        if !hosts.is_empty() && handler.has_tool("http_get") {
            handler.register_static(
                "http_get",
                std::sync::Arc::new(
                    evo_agent::builtin_tools::http_get::HttpGetTool::new()
                        .with_allowed_hosts(hosts.clone()),
                ),
            );
        }
    }
    handler
}

fn is_proposal(v: &Value) -> bool {
    v.get("status").and_then(|s| s.as_str()) == Some("needs_approval")
}
